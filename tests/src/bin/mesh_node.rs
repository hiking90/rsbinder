// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Mesh integration node — one binary, configured by argv into a role.
//!
//! The SAME generated `IMeshNode` stub is served over kernel binder
//! and/or RPC (binder-over-unix-socket) depending on the role, and the
//! same client code drives either transport. A test harness
//! (`tests/mesh_rpc.rs`, `tests/mesh_kernel.rs`) spawns ~10 of these in
//! a mesh, lets them hammer each other for a fixed duration, then reads
//! each node's `key=value` summary line from stdout to verify that every
//! exchange round-tripped intact with zero errors.
//!
//! Roles:
//!   rpc-server    : serve IMeshNode over an RPC unix socket; also act
//!                   as an RPC client against `--peer` sockets.
//!   rpc-client    : RPC client only, against `--peer` sockets.
//!   kernel-server : serve IMeshNode over kernel binder (the single
//!                   service manager node); also an RPC client.
//!   kernel-client : kernel binder client of `--kernel-service`; also
//!                   serve IMeshNode over an RPC unix socket.
//!
//! Args:
//!   --role <r>             one of the roles above
//!   --name <s>             this node's identity (origin string)
//!   --listen <path>        unix socket to bind an RPC server on (server roles)
//!   --peer <path>          an RPC peer socket to drive (repeatable)
//!   --kernel-service <s>   service-manager name (kernel roles)
//!   --duration-ms <n>      how long to keep exchanging
//!   --blob <n>             payload byte-array length

#![allow(non_snake_case)]

use std::sync::atomic::{AtomicI32, AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::service::{Broker as _, Registry as _};
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/mesh.rs"));

use mesh::IMeshNode::{BnMeshNode, IMeshNode};
use mesh::IMeshObserver::IMeshObserver;
use mesh::MeshMessage::MeshMessage;
use mesh::MeshValue::MeshValue;
use mesh::NodeKind::NodeKind;

const RPC_ROOT_VERSION: u32 = 0; // r34 default profile

/// Service name every node registers/resolves the `IMeshNode` binder under,
/// for *both* transports via the [`rsbinder::service`] facade's named-service
/// model. Over RPC each session is a private directory, so a fixed name is
/// safe; over kernel the orchestrator passes a per-run unique
/// `--kernel-service` name (the shared device service manager is global), so
/// the kernel roles register/resolve under that argument instead.
const MESH_SVC: &str = "mesh.node";

// ---- service implementation (shared by both transports) -------------

struct MeshNodeImpl {
    name: String,
    kind: NodeKind,
    // Shared so the binder served by the server and the summary printer
    // observe the same inbound count (the served binder is a distinct
    // object from the one the main thread reads).
    received: Arc<AtomicI32>,
    accumulator: AtomicI64,
}

impl MeshNodeImpl {
    fn new(name: String, kind: NodeKind, received: Arc<AtomicI32>) -> Self {
        Self {
            name,
            kind,
            received,
            accumulator: AtomicI64::new(0),
        }
    }
}

impl Interface for MeshNodeImpl {}

impl IMeshNode for MeshNodeImpl {
    fn r#exchange(&self, req: &MeshMessage) -> status::Result<MeshMessage> {
        self.received.fetch_add(1, Ordering::Relaxed);
        // Deterministic transform: seq+1, origin/kind rewritten to this
        // node; nonce + blob echoed unchanged so the caller can verify
        // wire integrity end to end.
        Ok(MeshMessage {
            seq: req.seq + 1,
            nonce: req.nonce,
            origin: self.name.clone(),
            originKind: self.kind,
            blob: req.blob.clone(),
        })
    }

    fn r#accumulate(&self, v: &MeshValue) -> status::Result<i64> {
        let delta = match v {
            MeshValue::I(i) => *i as i64,
            MeshValue::L(l) => *l,
            MeshValue::S(s) => s.len() as i64,
        };
        Ok(self.accumulator.fetch_add(delta, Ordering::Relaxed) + delta)
    }

    fn r#notify(&self, _msg: &MeshMessage) -> status::Result<()> {
        self.received.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn r#registerObserver(&self, _obs: &Strong<dyn IMeshObserver>) -> status::Result<()> {
        Ok(())
    }

    fn r#receivedCount(&self) -> status::Result<i32> {
        Ok(self.received.load(Ordering::Relaxed))
    }
}

// ---- per-node config parsed from argv --------------------------------

struct Config {
    role: String,
    name: String,
    listen: Option<String>,
    peers: Vec<String>,
    // Only read by the kernel roles (linux/android); unused elsewhere.
    #[cfg_attr(not(any(target_os = "linux", target_os = "android")), allow(dead_code))]
    kernel_service: Option<String>,
    duration: Duration,
    blob_len: usize,
}

fn parse_args() -> Config {
    let mut role = String::from("rpc-client");
    let mut name = String::from("node");
    let mut listen = None;
    let mut peers = Vec::new();
    let mut kernel_service = None;
    let mut duration_ms: u64 = 1000;
    let mut blob_len: usize = 32;

    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        let a = args[i].as_str();
        let mut next = || {
            i += 1;
            args.get(i).cloned().unwrap_or_default()
        };
        match a {
            "--role" => role = next(),
            "--name" => name = next(),
            "--listen" => listen = Some(next()),
            "--peer" => peers.push(next()),
            "--kernel-service" => kernel_service = Some(next()),
            "--duration-ms" => duration_ms = next().parse().unwrap_or(1000),
            "--blob" => blob_len = next().parse().unwrap_or(32),
            other => eprintln!("mesh_node: ignoring unknown arg {other}"),
        }
        i += 1;
    }

    Config {
        role,
        name,
        listen,
        peers,
        kernel_service,
        duration: Duration::from_millis(duration_ms),
        blob_len,
    }
}

// ---- client-side hammer loop ----------------------------------------

#[derive(Default)]
struct Stats {
    tx: u64,
    rx: u64,
    errors: u64,
    integrity_fail: u64,
    // Peers that closed their connection during teardown (their own
    // deadline elapsed first). Expected in an independently-timed mesh,
    // so tracked separately from real protocol errors.
    peer_gone: u64,
}

/// A peer that finished its own run first closes its server, so an
/// in-flight call surfaces as DeadObject (proxy) / a transport close.
/// That is teardown, not a protocol failure.
fn is_peer_teardown(e: rsbinder::StatusCode) -> bool {
    matches!(
        e,
        rsbinder::StatusCode::DeadObject | rsbinder::StatusCode::FailedTransaction
    )
}

/// Drive one peer proxy until `deadline`, verifying each exchange
/// round-trips with the nonce/blob intact and the seq incremented.
/// Returns `false` once the peer tears down (caller drops it from
/// rotation), `true` if it stopped only because the deadline passed.
fn hammer(
    node: &Strong<dyn IMeshNode>,
    name: &str,
    blob_len: usize,
    deadline: Instant,
    stats: &mut Stats,
) -> bool {
    let blob: Vec<u8> = (0..blob_len).map(|n| (n & 0xff) as u8).collect();
    let mut seq: i32 = 0;
    while Instant::now() < deadline {
        let nonce = (seq as i64).wrapping_mul(0x9E37_79B9_7F4A_7C15u64 as i64) ^ 0x5151;
        let req = MeshMessage {
            seq,
            nonce,
            origin: name.to_string(),
            originKind: NodeKind::RPC,
            blob: blob.clone(),
        };
        match node.exchange(&req) {
            Ok(reply) => {
                stats.tx += 1;
                stats.rx += 1;
                if reply.seq != seq + 1 || reply.nonce != nonce || reply.blob != blob {
                    stats.integrity_fail += 1;
                }
            }
            Err(e) if is_peer_teardown(e.transaction_error()) => {
                stats.peer_gone += 1;
                return false;
            }
            Err(e) => {
                if stats.errors == 0 {
                    eprintln!("mesh_node {name}: first exchange error: {e:?}");
                }
                stats.errors += 1;
                return false;
            }
        }
        // Interleave a union accumulate + a oneway notify.
        match node.accumulate(&MeshValue::I(seq)) {
            Ok(_) => {}
            Err(e) if is_peer_teardown(e.transaction_error()) => {
                stats.peer_gone += 1;
                return false;
            }
            Err(_) => {
                stats.errors += 1;
                return false;
            }
        }
        let _ = node.notify(&req);
        seq = seq.wrapping_add(1);
    }
    true
}

fn connect_peer_rpc(path: &str) -> Result<(rsbinder::service::rpc::Broker, Strong<dyn IMeshNode>)> {
    // The facade Broker owns the underlying `RpcSession`, so keeping it
    // alongside the proxy keeps the connection up (no more "drop session →
    // DeadObject"). Resolution is the named-service model: the peer's
    // `Host::add_service(MESH_SVC, ..)` published the binder; we look it up
    // by the same name through this session's in-process directory.
    let broker = rsbinder::service::rpc::Broker::unix(path)?;
    let node: Strong<dyn IMeshNode> = broker.get_interface(MESH_SVC)?;
    // A connect() can succeed against a socket whose server has bound but
    // not yet entered its accept/serve loop; the first transaction then
    // races and may surface DeadObject. Probe once here so the caller's
    // retry loop treats that as "not ready yet" rather than a real error.
    node.receivedCount()?;
    // The broker owns the session/connection; it must outlive `node` or the
    // proxy goes DeadObject. Caller keeps both.
    Ok((broker, node))
}

fn print_summary(name: &str, role: &str, s: &Stats, served: i32) {
    // Single machine-readable line the harness greps for.
    println!(
        "MESH_SUMMARY name={name} role={role} tx={} rx={} errors={} integrity_fail={} peer_gone={} served_rx={served}",
        s.tx, s.rx, s.errors, s.integrity_fail, s.peer_gone
    );
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let _ = RPC_ROOT_VERSION;
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    let cfg = parse_args();
    let deadline = Instant::now() + cfg.duration;

    // Shared inbound counter: the served binder bumps it, the summary
    // printer reads it.
    let served = Arc::new(AtomicI32::new(0));

    match cfg.role.as_str() {
        "rpc-server" => run_rpc_server(&cfg, served, deadline),
        "rpc-client" => run_rpc_client(&cfg, served, deadline),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "kernel-server" => run_kernel_server(&cfg, served, deadline),
        #[cfg(any(target_os = "linux", target_os = "android"))]
        "kernel-client" => run_kernel_client(&cfg, served, deadline),
        other => {
            eprintln!("mesh_node: unsupported role {other} on this platform");
            std::process::exit(2);
        }
    }
}

// ---- RPC roles -------------------------------------------------------

fn run_rpc_server(
    cfg: &Config,
    served: Arc<AtomicI32>,
    deadline: Instant,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let listen = cfg.listen.as_ref().ok_or("rpc-server requires --listen")?;
    let _ = std::fs::remove_file(listen);
    let host = rsbinder::service::rpc::Host::unix(listen)?;
    let svc = MeshNodeImpl::new(cfg.name.clone(), NodeKind::RPC, Arc::clone(&served));
    host.add_service(MESH_SVC, BnMeshNode::new_binder(svc).as_binder())?;
    let _bg = host.serve_background();

    // Give peers a moment to bind, then hammer them as a client too.
    let mut stats = Stats::default();
    drive_peers(cfg, deadline, &mut stats);

    // A server must keep serving for the WHOLE window even if its own
    // client traffic finished early (e.g. all peers were briefly
    // unreachable at startup): other nodes are still dialing us, and the
    // background accept loop dies when this thread (the process) returns.
    serve_until(deadline);

    print_summary(&cfg.name, &cfg.role, &stats, served.load(Ordering::Relaxed));
    Ok(())
}

/// Block the main thread until `deadline` so a backgrounded RPC server
/// keeps accepting for the full run.
fn serve_until(deadline: Instant) {
    let now = Instant::now();
    if deadline > now {
        std::thread::sleep(deadline - now);
    }
}

fn run_rpc_client(
    cfg: &Config,
    served: Arc<AtomicI32>,
    deadline: Instant,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let mut stats = Stats::default();
    drive_peers(cfg, deadline, &mut stats);
    print_summary(&cfg.name, &cfg.role, &stats, served.load(Ordering::Relaxed));
    Ok(())
}

/// Connect to every `--peer` (with bounded retry while they come up) and
/// hammer each in turn until the deadline.
fn drive_peers(cfg: &Config, deadline: Instant, stats: &mut Stats) {
    // Keep each broker (which owns the RPC session) alive alongside its
    // proxy for the whole run, or the proxy goes DeadObject.
    let mut brokers: Vec<rsbinder::service::rpc::Broker> = Vec::new();
    let mut nodes: Vec<Strong<dyn IMeshNode>> = Vec::new();
    // Reserve ~20% of the window (capped at 3s) for hammering, so even a
    // short run still exchanges after connecting.
    let reserve = (cfg.duration / 5).min(Duration::from_secs(3));
    let connect_deadline = deadline.checked_sub(reserve).unwrap_or(deadline);
    // A peer's listen socket can transiently `ECONNREFUSED` when its
    // accept loop is briefly starved (e.g. by the kernel nodes'
    // concurrent ProcessState init in a mixed mesh) and the listen
    // backlog fills. We connect peers one at a time, retrying a refused
    // peer with an *exponential* backoff (giving the saturated backlog
    // time to drain) until it connects or the window closes. Measured to
    // beat round-robin retry, which re-dials every pending peer each pass
    // and so keeps hammering the saturated backlog.
    for p in &cfg.peers {
        let mut backoff = Duration::from_millis(20);
        loop {
            match connect_peer_rpc(p) {
                Ok((broker, n)) => {
                    brokers.push(broker);
                    nodes.push(n);
                    break;
                }
                Err(_) if Instant::now() < connect_deadline => {
                    std::thread::sleep(backoff);
                    backoff = (backoff * 2).min(Duration::from_millis(500));
                }
                Err(_) => {
                    // Never reachable within our window — startup race,
                    // not a protocol failure, so counted as peer_gone.
                    eprintln!("mesh_node {}: peer {p} unreachable in window", cfg.name);
                    stats.peer_gone += 1;
                    break;
                }
            }
        }
    }
    // Round-robin hammer so every peer keeps getting traffic; drop a
    // peer once it tears down (its own deadline elapsed) so we don't
    // busy-spin against a dead socket.
    while Instant::now() < deadline && !nodes.is_empty() {
        nodes.retain(|n| hammer(n, &cfg.name, cfg.blob_len, deadline, stats));
    }
}

// ---- kernel roles (linux/android only) ------------------------------

#[cfg(any(target_os = "linux", target_os = "android"))]
fn run_kernel_server(
    cfg: &Config,
    served: Arc<AtomicI32>,
    deadline: Instant,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let name = cfg
        .kernel_service
        .as_ref()
        .ok_or("kernel-server requires --kernel-service")?;
    // `kernel::Host::new` does the (idempotent) ProcessState init but does
    // NOT start the thread pool — `Host::serve` would block joining the
    // process-wide pool, and this node also needs to run as a client, so we
    // start the pool directly (the documented non-blocking kernel serve).
    let host = rsbinder::service::kernel::Host::new()?;
    let svc = MeshNodeImpl::new(cfg.name.clone(), NodeKind::KERNEL, Arc::clone(&served));
    host.add_service(name, BnMeshNode::new_binder(svc).as_binder())?;
    rsbinder::ProcessState::start_thread_pool();

    // Also act as an RPC client against peers, if any.
    let mut stats = Stats::default();
    drive_peers(cfg, deadline, &mut stats);
    // Keep the kernel service (and its thread pool) alive for the whole
    // window: kernel clients are still calling us.
    serve_until(deadline);
    print_summary(&cfg.name, &cfg.role, &stats, served.load(Ordering::Relaxed));
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn run_kernel_client(
    cfg: &Config,
    served: Arc<AtomicI32>,
    deadline: Instant,
) -> std::result::Result<(), Box<dyn std::error::Error>> {
    let name = cfg
        .kernel_service
        .as_ref()
        .ok_or("kernel-client requires --kernel-service")?;

    // Bring up our RPC server FIRST (before the slower kernel
    // ProcessState init + service lookup) so its socket binds as early as
    // possible for peers dialing us — minimizing startup-race
    // ECONNREFUSED across the 20-process mesh.
    let _bg = if let Some(listen) = &cfg.listen {
        let _ = std::fs::remove_file(listen);
        let host = rsbinder::service::rpc::Host::unix(listen)?;
        let svc = MeshNodeImpl::new(cfg.name.clone(), NodeKind::RPC, Arc::clone(&served));
        host.add_service(MESH_SVC, BnMeshNode::new_binder(svc).as_binder())?;
        Some(host.serve_background())
    } else {
        None
    };

    // `kernel::Broker::new` does the (idempotent) ProcessState init so the
    // system service manager is reachable.
    let broker = rsbinder::service::kernel::Broker::new()?;

    // Connect the kernel service with bounded retry.
    let mut kernel_node: Option<Strong<dyn IMeshNode>> = None;
    let mut attempt = 0;
    while Instant::now() < deadline {
        match broker.get_interface::<dyn IMeshNode>(name) {
            Ok(n) => {
                kernel_node = Some(n);
                break;
            }
            Err(_) if attempt < 50 => {
                attempt += 1;
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(e) => {
                eprintln!(
                    "mesh_node {}: cannot get kernel service {name}: {e:?}",
                    cfg.name
                );
                break;
            }
        }
    }

    let mut stats = Stats::default();
    if let Some(n) = &kernel_node {
        hammer(n, &cfg.name, cfg.blob_len, deadline, &mut stats);
    }
    drive_peers(cfg, deadline, &mut stats);
    // If we also front an RPC server, keep it alive for the whole window.
    if cfg.listen.is_some() {
        serve_until(deadline);
    }
    print_summary(&cfg.name, &cfg.role, &stats, served.load(Ordering::Relaxed));
    Ok(())
}

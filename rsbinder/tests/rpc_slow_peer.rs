// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Links and peers that are slow, not gone (plan 2-25).
//!
//! `rpc_link_break` covers a link that stops carrying anything. This file
//! covers the cases where bytes keep moving, slowly, and checks the two
//! deadlines whose meaning depends on how progress is counted:
//!
//! - **The send deadline** (`set_timeout`'s `SO_SNDTIMEO`, at most `d`
//!   without progress) must not cut a peer that keeps taking bytes, however
//!   slowly, and must still cut one that takes none. The kernel reports a
//!   socket writable again only once a share of its send buffer has drained
//!   (TCP: free space at least half of what is queued), and TCP grows that
//!   buffer to several megabytes on a long queue, so "a send call accepted
//!   bytes" can lag the peer's progress by seconds. The `in_ns_*` cases shape
//!   loopback with `tc netem` in a network namespace of their own, one per
//!   case (netem acts on every packet of the interface), the way
//!   `rpc_link_break` runs under `unshare -rn`. The Unix-domain case needs no
//!   namespace: a relay that reads slowly is a slow peer.
//! - **The handshake deadline** (`RpcServer::set_handshake_timeout`, and the
//!   client's `timeout` for each handshake step) is a bound on the whole
//!   phase. A peer that sends one byte at a time, each well inside `d`, must
//!   not stretch it. netem shapes whole segments, so a relay that forwards one
//!   byte per interval stands in for that peer.
//!
//! A host that refuses unprivileged user namespaces or has no `unshare`,
//! `tc` or `ip` skips the `in_ns_*` cases, saying so; with
//! `RSB_SLOW_LINK_REQUIRED` set (CI) it fails instead.

#![cfg(all(unix, feature = "rpc-tcp-debug", feature = "rpc-tls"))]

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::rpc::rustls::pki_types::pem::PemObject;
use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
use rsbinder::rpc::transport::TcpDebugTransport;
use rsbinder::rpc::{RpcClientConfig, RpcProxy, RpcServer, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION, FLAG_ONEWAY,
};

const DESC: &str = "rsbinder.test.ISlowPeer";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_BIG: TransactionCode = FIRST_CALL_TRANSACTION + 1;

const CA: &str = include_str!("tls_fixtures/ca.crt");
const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

/// The deadline every case arms.
const D: Duration = Duration::from_secs(1);
/// The relay's pace for a trickling peer: each byte well inside `D`.
const TRICKLE_EVERY: Duration = Duration::from_millis(300);
/// How late after `D` a whole-phase deadline may act on a loaded host.
const SLACK: Duration = Duration::from_millis(1500);

struct Svc;
impl Interface for Svc {}
impl Remotable for Svc {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(&self, code: TransactionCode, r: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TX_ECHO => {
                let payload: Vec<u8> = r.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&(payload.len() as i32))
            }
            TX_BIG => {
                let len: i32 = r.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&vec![0x5au8; len.max(0) as usize])
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<std::result::Result<_, _>>()
        .expect("parse certs")
}

fn server_config() -> Arc<ServerConfig> {
    let key = PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("parse key");
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(certs(SRV_CRT), key)
            .expect("server config"),
    )
}

fn client_config() -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    for c in certs(CA) {
        roots.add(c).expect("add ca");
    }
    Arc::new(
        ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth(),
    )
}

fn tmp_sock(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "rsb_slow_{tag}_{}_{nanos}.sock",
        std::process::id()
    ))
}

/// A loopback port nothing listens on right now.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .and_then(|l| l.local_addr())
        .expect("free port")
        .port()
}

fn root_proxy(root: &SIBinder) -> &RpcProxy {
    (**root)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("RpcProxy")
}

fn echo(root: &SIBinder, payload: &[u8], flags: u32) -> Result<()> {
    let proxy = root_proxy(root);
    let mut d = proxy.build_request(DESC)?;
    d.write(&payload.to_vec())?;
    proxy.transact(TX_ECHO, &d, flags).map(|_| ())
}

fn big_reply(root: &SIBinder, len: usize) -> Result<usize> {
    let proxy = root_proxy(root);
    let mut d = proxy.build_request(DESC)?;
    d.write(&(len as i32))?;
    let mut reply = proxy
        .transact(TX_BIG, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let status: Status = reply.read()?;
    assert!(status.is_ok(), "{status:?}");
    let body: Vec<u8> = reply.read()?;
    Ok(body.len())
}

/// Run `f` on a thread and wait at most `wait` for it; `None` if it is still going.
fn within<T: Send + 'static>(wait: Duration, f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(wait).ok()
}

// ---------------------------------------------------------------------------
// Servers
// ---------------------------------------------------------------------------

/// Where a test server listens.
#[derive(Clone, Debug)]
enum At {
    Unix(PathBuf),
    Tcp(SocketAddr),
}

/// A server for `Svc`; dropping it stops its listener.
struct Server {
    server: Arc<RpcServer>,
    at: At,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.server.terminate();
        if let At::Unix(path) = &self.at {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Wire {
    R34,
    A13,
}

/// How a server is built; `apply` runs before it serves.
struct ServerSpec {
    wire: Wire,
    handshake_timeout: Option<Duration>,
    reply_timeout: Option<Duration>,
    max_connections: Option<usize>,
    idle_timeout: Option<Duration>,
}

impl ServerSpec {
    fn a13() -> Self {
        ServerSpec {
            wire: Wire::A13,
            handshake_timeout: None,
            reply_timeout: None,
            max_connections: None,
            idle_timeout: None,
        }
    }

    fn apply(&self, server: &Arc<RpcServer>) {
        if self.wire == Wire::A13 {
            server.set_android13plus(2);
        }
        if let Some(d) = self.handshake_timeout {
            server.set_handshake_timeout(Some(d));
        }
        server.set_reply_timeout(self.reply_timeout);
        if let Some(n) = self.max_connections {
            server.set_max_connections(n);
        }
        server.set_idle_timeout(self.idle_timeout);
        server
            .set_root(Binder::new(Svc).as_binder())
            .expect("set_root");
    }

    fn unix(self) -> Server {
        let path = tmp_sock("srv");
        let server = RpcServer::setup_unix_server(&path).expect("unix server");
        self.apply(&server);
        let _ = server.run_background();
        Server {
            server,
            at: At::Unix(path),
        }
    }

    fn tcp_debug(self) -> Server {
        self.tcp_debug_on(0)
    }

    /// `tcp_debug` has no listener of its own: a raw accept loop hands each stream over.
    fn tcp_debug_on(self, port: u16) -> Server {
        // `serve_connection` needs a server object, whose own listener is never run.
        let path = tmp_sock("tcpdbg");
        let server = RpcServer::setup_unix_server(&path).expect("server object");
        let _ = std::fs::remove_file(&path);
        self.apply(&server);
        let listener = TcpListener::bind(("127.0.0.1", port)).expect("bind");
        let addr = listener.local_addr().expect("addr");
        let accepting = Arc::clone(&server);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { return };
                let Ok(t) = TcpDebugTransport::from_stream(stream) else {
                    return;
                };
                accepting.serve_connection(Box::new(t));
            }
        });
        Server {
            server,
            at: At::Tcp(addr),
        }
    }

    fn tls(self) -> Server {
        self.tls_on(free_port())
    }

    fn tls_on(self, port: u16) -> Server {
        let server =
            RpcServer::setup_tcp_server_tls(("127.0.0.1", port), server_config()).expect("tls");
        self.apply(&server);
        let _ = server.run_background();
        Server {
            server,
            at: At::Tcp(([127, 0, 0, 1], port).into()),
        }
    }
}

#[derive(Clone, Copy)]
enum Link {
    Unix,
    TcpDebug,
    Tls,
}

/// An android-13+ client of the server at `at` over `link`, with `timeout` as its `d`.
fn connect(link: Link, at: &At, timeout: Option<Duration>) -> Result<RpcSession> {
    let config = match (link, at) {
        (Link::Unix, At::Unix(path)) => RpcClientConfig::unix(path, 2),
        (Link::TcpDebug, At::Tcp(addr)) => RpcClientConfig::tcp_debug(*addr, 2),
        (Link::Tls, At::Tcp(addr)) => {
            RpcClientConfig::tls("127.0.0.1", addr.port(), "localhost", client_config(), 2)
        }
        _ => panic!("link and address disagree"),
    };
    let config = match timeout {
        Some(d) => config.timeout(d),
        None => config,
    };
    RpcSession::setup_client_android13plus_with_config(config)
}

// ---------------------------------------------------------------------------
// Relay
// ---------------------------------------------------------------------------

/// How the relay moves one direction's bytes.
#[derive(Clone)]
enum Pace {
    /// As fast as both sockets allow.
    Full,
    /// One byte per interval: a peer that never goes quiet for long.
    Trickle(Duration),
    /// At most `n` bytes per interval from the source: a reader that is slow but steady.
    Rate(usize, Duration),
    /// Full until the flag is set, then nothing more is read: a peer that stops reading.
    HoldWhen(Arc<AtomicBool>),
}

trait Duplex: Read + Write + Send + Sync + 'static {
    fn split(&self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>);
    fn close(&self);
}

impl Duplex for UnixStream {
    fn split(&self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
        let r = self.try_clone().expect("clone");
        let w = self.try_clone().expect("clone");
        (Box::new(r), Box::new(w))
    }
    fn close(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

impl Duplex for TcpStream {
    fn split(&self) -> (Box<dyn Read + Send>, Box<dyn Write + Send>) {
        let r = self.try_clone().expect("clone");
        let w = self.try_clone().expect("clone");
        (Box::new(r), Box::new(w))
    }
    fn close(&self) {
        let _ = self.shutdown(std::net::Shutdown::Both);
    }
}

/// A man in the middle between clients and the server at `upstream`, pacing each direction.
struct Relay {
    at: At,
    /// When the server's side of the first relayed connection ended (EOF or error).
    server_left: Arc<Mutex<Option<Instant>>>,
    started: Arc<Mutex<Option<Instant>>>,
}

impl Relay {
    /// `up` paces client → server, `down` server → client.
    fn start(upstream: &At, up: Pace, down: Pace) -> Relay {
        let server_left = Arc::new(Mutex::new(None));
        let started = Arc::new(Mutex::new(None));
        let at = match upstream {
            At::Unix(target) => {
                let path = tmp_sock("relay");
                let listener = UnixListener::bind(&path).expect("relay bind");
                let target = target.clone();
                let (left, st) = (server_left.clone(), started.clone());
                thread::spawn(move || {
                    for client in listener.incoming() {
                        let Ok(client) = client else { return };
                        let Ok(server) = UnixStream::connect(&target) else {
                            return;
                        };
                        st.lock().unwrap().get_or_insert(Instant::now());
                        pump_pair(client, server, up.clone(), down.clone(), left.clone());
                    }
                });
                At::Unix(path)
            }
            At::Tcp(target) => {
                let listener = TcpListener::bind("127.0.0.1:0").expect("relay bind");
                let addr = listener.local_addr().expect("addr");
                let target = *target;
                let (left, st) = (server_left.clone(), started.clone());
                thread::spawn(move || {
                    for client in listener.incoming() {
                        let Ok(client) = client else { return };
                        let Ok(server) = TcpStream::connect(target) else {
                            return;
                        };
                        let _ = server.set_nodelay(true);
                        let _ = client.set_nodelay(true);
                        st.lock().unwrap().get_or_insert(Instant::now());
                        pump_pair(client, server, up.clone(), down.clone(), left.clone());
                    }
                });
                At::Tcp(addr)
            }
        };
        Relay {
            at,
            server_left,
            started,
        }
    }

    /// How long after the first connection the server ended it, waiting up to `wait`.
    fn server_left_within(&self, wait: Duration) -> Option<Duration> {
        let deadline = Instant::now() + wait;
        loop {
            if let (Some(left), Some(started)) = (
                *self.server_left.lock().unwrap(),
                *self.started.lock().unwrap(),
            ) {
                return Some(left - started);
            }
            if Instant::now() > deadline {
                return None;
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}

fn pump_pair<S: Duplex>(
    client: S,
    server: S,
    up: Pace,
    down: Pace,
    server_left: Arc<Mutex<Option<Instant>>>,
) {
    let client = Arc::new(client);
    let server = Arc::new(server);
    let (cr, cw) = client.split();
    let (sr, sw) = server.split();
    {
        let (client, server) = (client.clone(), server.clone());
        thread::spawn(move || {
            pump(cr, sw, up);
            server.close();
            client.close();
        });
    }
    thread::spawn(move || {
        pump(sr, cw, down);
        // The server's side ended: its deadline, its refusal or its own close.
        server_left.lock().unwrap().get_or_insert(Instant::now());
        client.close();
        server.close();
    });
}

fn pump(mut from: Box<dyn Read + Send>, mut to: Box<dyn Write + Send>, pace: Pace) {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let limit = match &pace {
            Pace::Rate(n, _) => (*n).min(buf.len()),
            _ => buf.len(),
        };
        if let Pace::HoldWhen(flag) = &pace {
            if flag.load(Ordering::SeqCst) {
                // Read nothing more; the socket stays open, so the sender's buffers fill.
                thread::sleep(Duration::from_secs(3600));
                return;
            }
        }
        let n = match from.read(&mut buf[..limit]) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        let ok = match &pace {
            Pace::Trickle(every) => buf[..n].iter().all(|b| {
                thread::sleep(*every);
                to.write_all(std::slice::from_ref(b)).is_ok()
            }),
            _ => to.write_all(&buf[..n]).is_ok(),
        };
        if !ok {
            return;
        }
        if let Pace::Rate(_, every) = &pace {
            thread::sleep(*every);
        }
    }
}

// ---------------------------------------------------------------------------
// The send deadline: slow links (netem, one network namespace per case)
// ---------------------------------------------------------------------------

/// Set for the run inside a namespace; the `in_ns_*` cases do nothing without it.
const IN_NS: &str = "RSB_SLOW_LINK_IN_NS";
/// Set where a skip would hide a gap (CI): a host that cannot run the cases fails instead.
const REQUIRED: &str = "RSB_SLOW_LINK_REQUIRED";

/// The kernel wakes a waiting sender once about a third of a send buffer that has grown to
/// megabytes has drained: at 1 Mbit/s each wake-up takes several times `D`.
const SLOW_LINK: &str = "rate 1mbit delay 20ms";
/// About 17 s on `SLOW_LINK`: long enough for the send buffer to grow, and for the frame's tail
/// to sit in it, unread, for more than `D` after the send returned.
const BULK: usize = 2 * 1024 * 1024;
/// Under the time `BULK` needs: a faster result means the shaping did not apply.
const BULK_AT_LEAST: Duration = Duration::from_secs(4);

/// What the other direction gets: the delay, not the rate.
const REVERSE_LINK: &str = "delay 20ms";
/// The server's port inside a case's namespace, by which the shaping tells the directions apart.
const NS_PORT: u16 = 7000;
/// The `u32` match for the slow direction: towards the server (a request) or from it (a reply).
const TO_SERVER: &str = "dport";
const FROM_SERVER: &str = "sport";

/// `(case, slow direction)`: each runs in a namespace of its own.
const NS_CASES: &[(&str, &str)] = &[
    ("in_ns_tcp_large_request_over_a_slow_link", TO_SERVER),
    ("in_ns_tls_large_request_over_a_slow_link", TO_SERVER),
    ("in_ns_tcp_large_reply_over_a_slow_link", FROM_SERVER),
    ("in_ns_tls_large_reply_over_a_slow_link", FROM_SERVER),
    ("in_ns_tcp_large_reply_under_an_idle_deadline", FROM_SERVER),
];

/// Shell commands that shape loopback: `SLOW_LINK` one way, `REVERSE_LINK` the other.
///
/// One netem on `lo` would queue both directions together, so the receiver's ACKs would wait
/// behind the data they acknowledge, as on no real link, and `SIOCOUTQ` would stand still for
/// seconds while the link is busy. A `prio` qdisc with a `u32` port match keeps them apart.
#[cfg(target_os = "linux")]
fn shaping(slow: &str) -> String {
    format!(
        "ip link set lo up mtu 1500 \
         && tc qdisc add dev lo root handle 1: prio bands 3 \
            priomap 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2 2 \
         && tc qdisc add dev lo parent 1:1 handle 10: netem {SLOW_LINK} \
         && tc qdisc add dev lo parent 1:3 handle 30: netem {REVERSE_LINK} \
         && tc filter add dev lo parent 1: protocol ip prio 1 u32 \
            match ip {slow} {NS_PORT} 0xffff flowid 1:1"
    )
}

#[cfg(target_os = "linux")]
#[test]
fn slow_links_in_network_namespaces() {
    use std::process::{Command, Stdio};

    let refused = |what: &str| {
        assert!(
            std::env::var_os(REQUIRED).is_none(),
            "{REQUIRED} is set: {what}"
        );
        eprintln!("SKIP slow_links_in_network_namespaces: {what}");
    };
    match Command::new("unshare").args(["-rn", "true"]).status() {
        Err(e) => return refused(&format!("cannot run `unshare`: {e}")),
        Ok(s) if !s.success() => {
            return refused("`unshare -rn` is refused (unprivileged user namespaces off?)")
        }
        Ok(_) => {}
    }
    for tool in [["tc", "-V"], ["ip", "-V"]] {
        if let Err(e) = Command::new(tool[0]).arg(tool[1]).output() {
            return refused(&format!("cannot run `{}`: {e}", tool[0]));
        }
    }
    // A namespace cannot load a qdisc module the host lacks: try the shaping once first.
    let shapes = Command::new("unshare")
        .args(["-rn", "sh", "-c", &shaping(TO_SERVER)])
        .output();
    match shapes {
        Ok(o) if o.status.success() => {}
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            return refused(&format!(
                "cannot shape loopback (sch_prio, cls_u32, sch_netem loaded?): {err}"
            ));
        }
        Err(e) => return refused(&format!("cannot run `unshare`: {e}")),
    }
    let exe = std::env::current_exe().expect("current_exe");
    let runs: Vec<_> = NS_CASES
        .iter()
        .map(|(case, slow)| {
            let out = std::env::temp_dir()
                .join(format!("rsb_slow_link_{}_{case}.out", std::process::id()));
            let log = std::fs::File::create(&out).expect("create the case's output file");
            // `$0` is this binary, `$1` the case.
            let setup = format!("{} && exec \"$0\" --ignored --exact \"$1\"", shaping(slow));
            let child = Command::new("unshare")
                .args(["-rn", "sh", "-c", &setup])
                .arg(&exe)
                .arg(case)
                .env(IN_NS, "1")
                .stdout(Stdio::from(log.try_clone().expect("clone log")))
                .stderr(Stdio::from(log))
                .spawn()
                .expect("spawn the run in a namespace");
            (case, child, out)
        })
        .collect();
    // Bounded: a case that hangs would otherwise hang the suite and report nothing.
    let deadline = Instant::now() + Duration::from_secs(150);
    let mut failed = Vec::new();
    for (case, mut child, out) in runs {
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break Some(status);
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                let _ = child.wait();
                break None;
            }
            thread::sleep(Duration::from_millis(100));
        };
        let printed = std::fs::read_to_string(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        println!("--- {case}\n{printed}");
        let ran = printed.contains("test result: ok. 1 passed");
        if !matches!(status, Some(s) if s.success()) || !ran {
            failed.push(format!("{case}: {status:?}"));
        }
    }
    assert!(failed.is_empty(), "cases failed or hung: {failed:?}");
}

fn in_ns() -> bool {
    let inside = std::env::var_os(IN_NS).is_some();
    if !inside {
        eprintln!("run by slow_links_in_network_namespaces, inside a network namespace");
    }
    inside
}

/// A request far larger than the send buffer, to a server that reads it at link speed.
fn large_request_over_a_slow_link(link: Link) {
    let server = ns_server(link, ServerSpec::a13());
    let session = connect(link, &server.at, Some(D)).expect("connect");
    let root = session.get_root().expect("root");
    let payload = vec![7u8; BULK];
    let t0 = Instant::now();
    let got = echo(&root, &payload, 0);
    let took = t0.elapsed();
    assert_eq!(
        got,
        Ok(()),
        "a {BULK}-byte request over a live {SLOW_LINK} link failed after {took:?}; ended={}",
        session.is_ended()
    );
    assert!(
        took >= BULK_AT_LEAST,
        "took {took:?}: the link was not shaped"
    );
}

/// A reply far larger than the send buffer: the server's own send, under its reply deadline.
fn large_reply_over_a_slow_link(link: Link) {
    large_reply(
        link,
        ServerSpec {
            reply_timeout: Some(D),
            ..ServerSpec::a13()
        },
    );
}

/// The server's idle wait starts while the reply's tail is still queued: the client is not idle.
fn large_reply_under_an_idle_deadline(link: Link) {
    large_reply(
        link,
        ServerSpec {
            idle_timeout: Some(D),
            ..ServerSpec::a13()
        },
    );
}

/// A server on `NS_PORT`, the port the namespace's shaping matches.
fn ns_server(link: Link, spec: ServerSpec) -> Server {
    match link {
        Link::TcpDebug => spec.tcp_debug_on(NS_PORT),
        Link::Tls => spec.tls_on(NS_PORT),
        Link::Unix => unreachable!("netem shapes IP only"),
    }
}

fn large_reply(link: Link, spec: ServerSpec) {
    let server = ns_server(link, spec);
    let session = connect(link, &server.at, Some(D)).expect("connect");
    let root = session.get_root().expect("root");
    let t0 = Instant::now();
    let got = big_reply(&root, BULK);
    let took = t0.elapsed();
    assert_eq!(
        got,
        Ok(BULK),
        "a {BULK}-byte reply over a live {SLOW_LINK} link failed after {took:?}; ended={}",
        session.is_ended()
    );
    assert!(
        took >= BULK_AT_LEAST,
        "took {took:?}: the link was not shaped"
    );
    // A server that gave up still delivers what it queued: only the next call tells.
    assert_eq!(
        echo(&root, &[1; 16], 0),
        Ok(()),
        "the server ended the session while the reply was still on its way"
    );
}

macro_rules! ns_cases {
    ($($name:ident => $body:expr;)*) => {$(
        #[test]
        #[ignore = "run inside a network namespace by slow_links_in_network_namespaces"]
        fn $name() {
            if in_ns() {
                $body;
            }
        }
    )*};
}

ns_cases! {
    in_ns_tcp_large_request_over_a_slow_link => large_request_over_a_slow_link(Link::TcpDebug);
    in_ns_tls_large_request_over_a_slow_link => large_request_over_a_slow_link(Link::Tls);
    in_ns_tcp_large_reply_over_a_slow_link => large_reply_over_a_slow_link(Link::TcpDebug);
    in_ns_tls_large_reply_over_a_slow_link => large_reply_over_a_slow_link(Link::Tls);
    in_ns_tcp_large_reply_under_an_idle_deadline =>
        large_reply_under_an_idle_deadline(Link::TcpDebug);
}

// ---------------------------------------------------------------------------
// The send deadline: slow and stalled readers (hermetic)
// ---------------------------------------------------------------------------

/// A Unix-domain peer that reads 16 KiB every 100 ms is slow but steady: not cut at `d`.
///
/// The kernel reports the socket writable again only once its write
/// allocation is down to a quarter of `SO_SNDBUF` (about 150 KiB of payload
/// to drain at the default 208 KiB), which this pace takes about 0.9 s.
#[cfg(any(target_os = "linux", target_os = "android"))]
#[test]
fn unix_steady_slow_reader_is_not_cut() {
    let d = Duration::from_millis(500);
    let server = ServerSpec::a13().unix();
    let relay = Relay::start(
        &server.at,
        Pace::Rate(16 * 1024, Duration::from_millis(100)),
        Pace::Full,
    );
    let session = connect(Link::Unix, &relay.at, Some(d)).expect("connect");
    let root = session.get_root().expect("root");
    let payload = vec![7u8; 1024 * 1024];
    let t0 = Instant::now();
    let got = echo(&root, &payload, 0);
    let took = t0.elapsed();
    assert_eq!(
        got,
        Ok(()),
        "a steady reader was cut after {took:?}; ended={}",
        session.is_ended()
    );
    assert!(
        took >= Duration::from_secs(3),
        "took {took:?}: the relay did not pace"
    );
}

/// The other side of the rule: a peer that stops reading is still cut, near `d`.
fn stalled_reader_is_cut(link: Link) {
    let spec = ServerSpec::a13();
    let server = match link {
        Link::Unix => spec.unix(),
        Link::TcpDebug => spec.tcp_debug(),
        Link::Tls => spec.tls(),
    };
    let hold = Arc::new(AtomicBool::new(false));
    let relay = Relay::start(&server.at, Pace::HoldWhen(hold.clone()), Pace::Full);
    let session = connect(link, &relay.at, Some(D)).expect("connect");
    let root = session.get_root().expect("root");
    assert_eq!(
        echo(&root, &[1; 16], 0),
        Ok(()),
        "a healthy session answers"
    );
    hold.store(true, Ordering::SeqCst);
    // The relay notices the hold at its next read; the first frame wakes it.
    let payload = vec![0u8; 256 * 1024];
    let t0 = Instant::now();
    // On a thread: a send that never gives up must fail the case, not hang it.
    let flooding = root.clone();
    let failed = within(D + Duration::from_secs(6), move || loop {
        if let Err(e) = echo(&flooding, &payload, FLAG_ONEWAY) {
            break e;
        }
    })
    .expect("sends to a peer that reads nothing never failed");
    let after = t0.elapsed();
    assert!(
        after >= D,
        "failed after {after:?} ({failed:?}): not by the deadline"
    );
    assert!(
        after <= D + Duration::from_secs(6),
        "failed only after {after:?} ({failed:?})"
    );
    assert!(session.is_ended(), "a failed send ends the session");
}

/// A reply wait whose request the peer never takes ends near `d`: queued bytes that do not
/// move are no progress.
fn reply_wait_on_a_stalled_reader_ends(link: Link) {
    let spec = ServerSpec::a13();
    let server = match link {
        Link::Unix => spec.unix(),
        Link::TcpDebug => spec.tcp_debug(),
        Link::Tls => spec.tls(),
    };
    let hold = Arc::new(AtomicBool::new(false));
    let relay = Relay::start(&server.at, Pace::HoldWhen(hold.clone()), Pace::Full);
    let session = connect(link, &relay.at, Some(D)).expect("connect");
    let root = session.get_root().expect("root");
    assert_eq!(
        echo(&root, &[1; 16], 0),
        Ok(()),
        "a healthy session answers"
    );
    hold.store(true, Ordering::SeqCst);
    // The first frame wakes the relay into its hold; the second stays queued here.
    let _ = echo(&root, &[2; 16], FLAG_ONEWAY);
    thread::sleep(Duration::from_millis(100));
    let t0 = Instant::now();
    let waiting = root.clone();
    let got = within(D + Duration::from_secs(6), move || {
        echo(&waiting, &[3; 4096], 0)
    })
    .expect("a reply wait on a peer that takes nothing never ended");
    let after = t0.elapsed();
    assert!(got.is_err(), "a reply came from a peer that reads nothing");
    assert!(
        after >= D.mul_f32(0.9),
        "ended after {after:?} ({got:?}): not by the deadline"
    );
}

#[test]
fn unix_reply_wait_on_a_stalled_reader_ends() {
    reply_wait_on_a_stalled_reader_ends(Link::Unix);
}

#[test]
fn tcp_reply_wait_on_a_stalled_reader_ends() {
    reply_wait_on_a_stalled_reader_ends(Link::TcpDebug);
}

#[test]
fn tls_reply_wait_on_a_stalled_reader_ends() {
    reply_wait_on_a_stalled_reader_ends(Link::Tls);
}

#[test]
fn unix_stalled_reader_is_cut() {
    stalled_reader_is_cut(Link::Unix);
}

#[test]
fn tcp_stalled_reader_is_cut() {
    stalled_reader_is_cut(Link::TcpDebug);
}

#[test]
fn tls_stalled_reader_is_cut() {
    stalled_reader_is_cut(Link::Tls);
}

// ---------------------------------------------------------------------------
// The handshake deadline: trickling peers (hermetic)
// ---------------------------------------------------------------------------

/// A client that sends its handshake one byte at a time is dropped `d` after it connected.
fn server_cuts_a_trickled_handshake(server: Server, client: impl FnOnce(At) + Send + 'static) {
    let relay = Relay::start(&server.at, Pace::Trickle(TRICKLE_EVERY), Pace::Full);
    let at = relay.at.clone();
    thread::spawn(move || client(at));
    let left = relay
        .server_left_within(D + SLACK)
        .expect("the server still holds a trickled handshake");
    assert!(
        left >= D.mul_f32(0.8),
        "dropped after {left:?}: not by the deadline"
    );
}

#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn server_cuts_a_trickled_a13_handshake() {
    let server = ServerSpec {
        handshake_timeout: Some(D),
        ..ServerSpec::a13()
    }
    .unix();
    server_cuts_a_trickled_handshake(server, |at| {
        let _ = connect(Link::Unix, &at, None);
    });
}

#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn server_cuts_a_trickled_r34_first_frame() {
    let server = ServerSpec {
        wire: Wire::R34,
        handshake_timeout: Some(D),
        ..ServerSpec::a13()
    }
    .unix();
    server_cuts_a_trickled_handshake(server, |at| {
        let At::Unix(path) = at else { unreachable!() };
        if let Ok(session) = RpcSession::setup_unix_client(path) {
            let _ = session.get_root();
        }
    });
}

#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn server_cuts_a_trickled_tls_handshake() {
    let server = ServerSpec {
        handshake_timeout: Some(D),
        ..ServerSpec::a13()
    }
    .tls();
    server_cuts_a_trickled_handshake(server, |at| {
        let _ = connect(Link::Tls, &at, None);
    });
}

/// A trickler holds no admission slot past `d`: the next client gets in.
#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn a_trickled_handshake_does_not_hold_a_connection_slot() {
    let server = ServerSpec {
        handshake_timeout: Some(D),
        max_connections: Some(1),
        ..ServerSpec::a13()
    }
    .unix();
    let relay = Relay::start(&server.at, Pace::Trickle(TRICKLE_EVERY), Pace::Full);
    let trickler_at = relay.at.clone();
    thread::spawn(move || {
        let _ = connect(Link::Unix, &trickler_at, None);
    });
    // Let the trickler take the only slot first.
    let started = Instant::now();
    while relay.started.lock().unwrap().is_none() {
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "the trickler never connected"
        );
        thread::sleep(Duration::from_millis(10));
    }
    thread::sleep(Duration::from_millis(200));
    let at = server.at.clone();
    let got = within(D + SLACK + Duration::from_secs(1), move || {
        connect(Link::Unix, &at, Some(Duration::from_secs(10))).map(|s| {
            let root = s.get_root();
            s.close_session();
            root.map(|_| ())
        })
    });
    assert_eq!(got, Some(Ok(Ok(()))), "the slot stayed with the trickler");
}

/// As AOSP `RpcServer::establishConnection`: a session id not 32 bytes long is refused unread.
#[test]
fn server_refuses_a_session_id_size_other_than_32_at_once() {
    let server = ServerSpec {
        handshake_timeout: Some(Duration::from_secs(5)),
        ..ServerSpec::a13()
    }
    .unix();
    let At::Unix(path) = &server.at else {
        unreachable!()
    };
    let mut sock = UnixStream::connect(path).expect("connect");
    // RpcConnectionHeader v1: version, options, fd mode, reserved[8], sessionIdSize = 0xffff.
    let mut header = Vec::new();
    header.extend_from_slice(&1u32.to_le_bytes());
    header.extend_from_slice(&[0, 0]);
    header.extend_from_slice(&[0; 8]);
    header.extend_from_slice(&0xffffu16.to_le_bytes());
    sock.write_all(&header).expect("header");
    sock.set_read_timeout(Some(Duration::from_secs(10)))
        .expect("read timeout");
    let t0 = Instant::now();
    let mut byte = [0u8; 1];
    let got = sock.read(&mut byte);
    let took = t0.elapsed();
    assert!(
        matches!(got, Ok(0) | Err(_)),
        "the server answered a malformed header: {got:?}"
    );
    assert!(
        took < Duration::from_secs(1),
        "the server waited {took:?} for a 65535-byte session id"
    );
}

/// A server that answers the handshake one byte at a time fails the setup call after `d`.
fn client_times_out_a_trickled_handshake(link: Link) {
    let server = match link {
        Link::Unix => ServerSpec::a13().unix(),
        Link::Tls => ServerSpec::a13().tls(),
        Link::TcpDebug => ServerSpec::a13().tcp_debug(),
    };
    let relay = Relay::start(&server.at, Pace::Full, Pace::Trickle(TRICKLE_EVERY));
    let at = relay.at.clone();
    let t0 = Instant::now();
    let got = within(D + SLACK, move || connect(link, &at, Some(D)).map(|_| ()));
    let took = t0.elapsed();
    assert_eq!(
        got,
        Some(Err(StatusCode::TimedOut)),
        "setup over a trickled handshake, after {took:?}"
    );
    assert!(
        took >= D.mul_f32(0.8),
        "failed after {took:?}: not by the deadline"
    );
}

#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn client_times_out_a_trickled_a13_handshake() {
    client_times_out_a_trickled_handshake(Link::Unix);
}

#[test]
#[ignore = "plan 2-25 D3: the whole-phase handshake deadline lands in the next commit"]
fn client_times_out_a_trickled_tls_handshake() {
    client_times_out_a_trickled_handshake(Link::Tls);
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-11 AC-11.3 STAGE3 — the rsbinder side of the FD-over-RPC
//! real-libbinder gate.
//!
//! Stands up an android-13+ `RpcServer` on a Unix-domain socket with the
//! `Unix` fd transport mode enabled, and publishes a root whose
//! transactions move a `ParcelFileDescriptor` in **both** directions.
//! `cpp/rpc_fd_interop_launcher.cpp` (a real libbinder `ARpcSession`
//! client with `setFileDescriptorTransportMode(Unix)`) drives it via
//! `cpp/run_rpc_fd_interop.sh`.
//!
//! `argv[1]` = socket path (default `/data/local/tmp/rsfd.sock`);
//! `argv[2]` = max RPC wire version offered (default 2; libbinder
//! negotiates `min(its own, this)` — 1 on Android 15, 2 on Android 16).
//! Transactions: 1=echo(String)->String, 2=fd_len(PFD)->i32,
//! 3=give_fd()->PFD, 4=new_child()->IBinder,
//! 5=echo_holder(ParcelableHolder)->ParcelableHolder (relayed undecoded),
//! 6=node_count()->i32, 7=take_binder(IBinder)->i32,
//! 8=holder_retry(ParcelableHolder)->i32. No AIDL `Status` header, so the
//! gate isolates the parcel body.
//!
//! 7 and 8 drive the received-binder accounting of plan 2-23: a binder of
//! ours sent back as an argument (7), and one inside a holder whose decode
//! fails past it twice (8), must each cost the launcher's `timesSent` exactly
//! one `DEC_STRONG`, so its proxy, and our node, go once it drops them.
//!
//! 4–6 drive an undecoded `ParcelableHolder` relay of one of our binders:
//! v2 re-takes a reference for every local binder in the copied range
//! (AOSP `Parcel::appendFrom` from android-16.0.0_r4), older wires refuse
//! the relay with `BadType`. `node_count` reports
//! `RpcServer::live_session_node_count` so the launcher can compare the
//! server's live nodes before and after it drops its proxies.

use std::sync::{Arc, OnceLock};

use rsbinder::rpc::RpcServer;
use rsbinder::*;

/// Must match the launcher's `AIBinder_Class` descriptor.
const IFACE: &str = "rsbinder.test.IFdInterop";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
/// Arg direction: real libbinder → rsbinder fd; reply = its byte length.
const TX_FD_LEN: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// Reply direction: rsbinder → real libbinder fd carrying `GIVE_FD_PAYLOAD`.
const TX_GIVE_FD: TransactionCode = FIRST_CALL_TRANSACTION + 2;
/// The launcher asserts these exact bytes.
const GIVE_FD_PAYLOAD: &[u8] = b"from-rsbinder-fd-2-11";
/// Reply = a fresh local binder, so each accounting case owns its own node.
const TX_NEW_CHILD: TransactionCode = FIRST_CALL_TRANSACTION + 3;
/// Reads a `ParcelableHolder` and writes it back into the reply without decoding it.
const TX_ECHO_HOLDER: TransactionCode = FIRST_CALL_TRANSACTION + 4;
/// Reply = `RpcServer::live_session_node_count`.
const TX_NODE_COUNT: TransactionCode = FIRST_CALL_TRANSACTION + 5;
/// Reads one binder argument; reply = 1 if it is one of ours coming home, else 0.
const TX_TAKE_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 6;
/// Reads a holder of `RetryProbe` and decodes it twice; both fail after the binder.
const TX_HOLDER_RETRY: TransactionCode = FIRST_CALL_TRANSACTION + 7;
/// Must match the launcher's holder payload name.
const RETRY_PROBE: &str = "rsbinder.test.RetryProbe";

/// Reads a binder then two `i32`s; the launcher sends one, so each decode fails past the binder.
#[derive(Debug, Default)]
struct RetryProbe {
    binder: Option<SIBinder>,
    tail: [i32; 2],
}

impl ParcelableMetadata for RetryProbe {
    fn descriptor() -> &'static str {
        RETRY_PROBE
    }
}

impl Parcelable for RetryProbe {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.binder)?;
        parcel.write(&self.tail[0])?;
        parcel.write(&self.tail[1])
    }
    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
        self.binder = parcel.read()?;
        self.tail = [parcel.read()?, parcel.read()?];
        Ok(())
    }
}

static SERVER: OnceLock<Arc<RpcServer>> = OnceLock::new();

struct Interop;
impl Interface for Interop {}

impl Remotable for Interop {
    fn descriptor() -> &'static str {
        IFACE
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        eprintln!("[rsbinder-server] on_transact code={code:#x}");
        match code {
            TX_ECHO => {
                let s: String = reader.read()?;
                eprintln!("[rsbinder-server] echo({s:?})");
                reply.write(&s)
            }
            TX_FD_LEN => {
                use std::io::{Read as _, Seek as _};
                let pfd: ParcelFileDescriptor = reader.read()?;
                let mut f =
                    std::fs::File::from(pfd.as_ref().try_clone().map_err(|_| StatusCode::BadFd)?);
                f.rewind().ok();
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(|_| StatusCode::BadFd)?;
                eprintln!(
                    "[rsbinder-server] fd_len: read {} bytes from the real-libbinder fd",
                    buf.len()
                );
                reply.write(&(buf.len() as i32))
            }
            TX_GIVE_FD => {
                use std::io::{Seek as _, Write as _};
                let mut tf = unlinked_tempfile().map_err(|_| StatusCode::BadFd)?;
                tf.write_all(GIVE_FD_PAYLOAD)
                    .map_err(|_| StatusCode::BadFd)?;
                tf.rewind().map_err(|_| StatusCode::BadFd)?;
                eprintln!("[rsbinder-server] give_fd: handing an fd to real libbinder");
                reply.write(&ParcelFileDescriptor::new(tf))
            }
            TX_NEW_CHILD => {
                eprintln!("[rsbinder-server] new_child: handing out a fresh local binder");
                reply.write(&Interface::as_binder(&Binder::new(Interop)))
            }
            TX_ECHO_HOLDER => {
                let mut holder = ParcelableHolder::new(Stability::Local);
                holder.read_from_parcel(reader)?;
                let relayed = holder.write_to_parcel(reply);
                eprintln!("[rsbinder-server] echo_holder: undecoded relay -> {relayed:?}");
                relayed
            }
            TX_NODE_COUNT => {
                let n = SERVER
                    .get()
                    .map_or(-1, |s| s.live_session_node_count() as i32);
                eprintln!("[rsbinder-server] node_count={n}");
                reply.write(&n)
            }
            TX_TAKE_BINDER => {
                let b: Option<SIBinder> = reader.read()?;
                let home = b.as_ref().is_some_and(|b| !b.is_remote());
                eprintln!("[rsbinder-server] take_binder: home={home}");
                reply.write(&i32::from(home))
            }
            TX_HOLDER_RETRY => {
                let mut holder = ParcelableHolder::new(Stability::Local);
                holder.read_from_parcel(reader)?;
                let first = holder.get_parcelable::<RetryProbe>();
                let second = holder.get_parcelable::<RetryProbe>();
                eprintln!("[rsbinder-server] holder_retry: {first:?} then {second:?}");
                reply.write(&i32::from(first.is_err() && second.is_err()))
            }
            _ => {
                eprintln!("[rsbinder-server] unknown txn {code:#x}");
                Err(StatusCode::UnknownTransaction)
            }
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A read/write temp file, immediately unlinked (the open fd keeps it alive).
fn unlinked_tempfile() -> std::io::Result<std::fs::File> {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsfd_givefd_{}_{}.tmp",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let f = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(&p)?;
    let _ = std::fs::remove_file(&p);
    Ok(f)
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("rsbinder::rpc=debug"),
    )
    .init();

    let sock = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/data/local/tmp/rsfd.sock".to_string());
    let max_version: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(2);
    let _ = std::fs::remove_file(&sock);

    let server = RpcServer::setup_unix_server(&sock)?;
    server.set_android13plus(max_version);
    server.set_max_threads(1);
    // The handshake honours the client's `fileDescriptorTransportMode`
    // byte only because this is set; v0 stays `None` (AOSP forbids fds
    // there), v1+ carries fds over `SCM_RIGHTS`.
    server.set_supported_fd_modes(&[rsbinder::rpc::FileDescriptorTransportMode::Unix]);
    server.set_root(Interface::as_binder(&Binder::new(Interop)))?;
    let _ = SERVER.set(server.clone());

    println!("[rsbinder-server] READY android13plus(v{max_version}) fd=unix on {sock}");
    server.run()?;
    Ok(())
}

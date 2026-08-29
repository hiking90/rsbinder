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
//! Argv[1] = socket path (default `/data/local/tmp/rsfd.sock`);
//! argv[2] = max RPC wire version offered (default 2; libbinder
//! negotiates `min(its own, this)` — 1 on Android 15, 2 on Android 16).
//! Transactions: 1=echo(String)->String, 2=fd_len(PFD)->i32,
//! 3=give_fd()->PFD. No AIDL `Status` header, so the gate isolates the
//! parcel body.

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
    server.set_root(Interface::as_binder(&Binder::new(Interop)));

    println!("[rsbinder-server] READY android13plus(v{max_version}) fd=unix on {sock}");
    server.run()?;
    Ok(())
}

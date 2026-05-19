// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! G4(b) / subplan-2-8 / subplan-2-11 live interop harness — the
//! rsbinder side.
//!
//! Stands up an **android-13+ `RpcServer`** (the G4(a) opt-in profile,
//! `set_android13plus(argv[2])`: 0=v0/android-13, 1=v1/android-14·15,
//! 2=v2/android-16) on a Unix-domain socket and publishes a trivial
//! root. A **real compiled android libbinder** RPC client connects on
//! the emulator — proving rsbinder's `Android13PlusCodec` +
//! AOSP-faithful framing + versioned handshake + (subplan 2-11)
//! **FD-over-RPC v1+ Parcel body + `SCM_RIGHTS`** interoperate with the
//! *genuine* AOSP peer, not just hermetically (RPC_STATUS §"G4(b)" /
//! §2-8 / §2-11). FD support is opt-in via `set_supported_fd_modes`;
//! the C launcher requests `Unix` in the `RpcConnectionHeader`.
//!
//! ```text
//! cargo ndk -t arm64-v8a -p 33 build -p example-hello --features rpc \
//!     --bin g4b_interop_server
//! adb push <bin> /data/local/tmp/g4b_fdsrv
//! adb shell /data/local/tmp/g4b_fdsrv /data/local/tmp/g4bfd.sock 2   # v2
//! # then the /tmp/g4b_fd_client ARpcSession launcher (RPC_STATUS §2-11)
//! ```
//!
//! Argv[1] = socket path (default `/data/local/tmp/g4b.sock`);
//! argv[2] = max RPC wire version offered (default 0).
//! Transactions: 1=echo(String), 2=fd_len(PFD)->i32, 3=give_fd()->PFD.

use rsbinder::rpc::RpcServer;
use rsbinder::*;

/// Must match the descriptor the android client's `AIBinder_Class`
/// uses, so libbinder's `writeInterfaceToken` matches what rsbinder's
/// RPC server adapter expects (`consume_rpc_interface_token`).
const IFACE: &str = "rsbinder.g4b.IInterop";
/// `FIRST_CALL_TRANSACTION` — echo(String) -> String.
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
/// subplan 2-11: fd(ParcelFileDescriptor) -> i32 byte length read from
/// it (arg direction: real libbinder → rsbinder fd).
const TX_FD_LEN: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// subplan 2-11: () -> ParcelFileDescriptor with known content
/// (reply direction: rsbinder → real libbinder fd).
const TX_GIVE_FD: TransactionCode = FIRST_CALL_TRANSACTION + 2;
/// The exact bytes the server's `give_fd` fd carries (the C launcher
/// asserts it).
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
                // Harness wire is deliberately minimal — exactly one
                // String arg in, one String back (no AIDL `Status`
                // header) — so STAGE3 isolates the *parcel-body* +
                // interface-token interop vs the real peer, not the
                // AIDL Status convention.
                let s: String = reader.read()?;
                eprintln!("[rsbinder-server] echo({s:?}) from real android libbinder");
                reply.write(&s)?;
                Ok(())
            }
            TX_FD_LEN => {
                // subplan 2-11 AC-11.3 (arg direction): the real
                // libbinder client wrote a `ParcelFileDescriptor`
                // through its AOSP `[not-null|hasComm|TYPE|fdIndex]`
                // v1+ body + `SCM_RIGHTS`; rsbinder must read it back,
                // dup the live fd, and reply its byte length.
                use std::io::{Read as _, Seek as _};
                let pfd: ParcelFileDescriptor = reader.read()?;
                let mut f =
                    std::fs::File::from(pfd.as_ref().try_clone().map_err(|_| StatusCode::BadFd)?);
                f.rewind().ok();
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(|_| StatusCode::BadFd)?;
                eprintln!(
                    "[rsbinder-server] fd_len: read {} bytes from real-libbinder fd",
                    buf.len()
                );
                reply.write(&(buf.len() as i32))?;
                Ok(())
            }
            TX_GIVE_FD => {
                // subplan 2-11 AC-11.3 (reply direction): rsbinder
                // writes a `ParcelFileDescriptor` (AOSP v1+ body +
                // `SCM_RIGHTS`); the real libbinder client must dup +
                // read `GIVE_FD_PAYLOAD` back.
                use std::io::{Seek as _, Write as _};
                let mut tf = unlinked_tempfile().map_err(|_| StatusCode::BadFd)?;
                tf.write_all(GIVE_FD_PAYLOAD)
                    .map_err(|_| StatusCode::BadFd)?;
                tf.rewind().map_err(|_| StatusCode::BadFd)?;
                eprintln!("[rsbinder-server] give_fd: handing an fd to real libbinder");
                reply.write(&ParcelFileDescriptor::new(tf))?;
                Ok(())
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

/// A read/write temp file, immediately unlinked (the open fd keeps it
/// alive) — no `tempfile` crate dep in `example-hello`.
fn unlinked_tempfile() -> std::io::Result<std::fs::File> {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "g4b_givefd_{}_{}.tmp",
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
        .unwrap_or_else(|| "/data/local/tmp/g4b.sock".to_string());
    // argv[2] = max RPC_WIRE_PROTOCOL_VERSION offered: 0 = android-13
    // (v0, G4(b) STAGE1), 1 = android-14/15 (v1, subplan 2-8 D2),
    // 2 = android-16 (v2 object table, subplan 2-8 D3). libbinder
    // negotiates min(its_own_max, this); on the android-16 emulator
    // libbinder's own max is 2, so this selects the negotiated wire.
    // Default 0 keeps the original G4(b)-v0 harness behaviour.
    let max_version: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let _ = std::fs::remove_file(&sock);

    let server = RpcServer::setup_unix_server(&sock)?;
    // G4(a)/subplan-2-8 opt-in: speak the android-13+ versioned RPC
    // wire at the requested max version (v0/v1/v2).
    server.set_android13plus(max_version);
    // 1 ⇒ libbinder opens exactly one outgoing connection. rsbinder's
    // model is one-connection-per-session; android-13's
    // multiple-connections-per-session thread pool is a documented
    // future refinement (server.rs module doc) out of this v0 smoke's
    // scope, so cap the negotiation at a single connection.
    server.set_max_threads(1);
    // subplan 2-11 Phase A0/D: opt in to `Unix` FD-over-RPC. The AOSP
    // handshake (`accept_android13plus_fd`) honors the client's
    // `RpcConnectionHeader.fileDescriptorTransportMode` byte only
    // because this is set; at v0 it stays `None` (v0 forbids fd —
    // AOSP-faithful), at v1/v2 fds ride `SCM_RIGHTS` over the AOSP
    // no-length-prefix framing with the `[not-null|hasComm|TYPE|idx]`
    // body. Harmless at v0 / for the String-only STAGE3 path.
    server.set_supported_fd_modes(&[rsbinder::rpc::FileDescriptorTransportMode::Unix]);
    server.set_root(Interface::as_binder(&Binder::new(Interop)));

    println!("[rsbinder-server] READY android13plus(v{max_version}) fd=unix on {sock}");
    // Block in the accept loop until killed.
    server.run()?;
    Ok(())
}

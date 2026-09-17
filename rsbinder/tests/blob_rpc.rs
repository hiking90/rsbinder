// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-2 AC-2.4 / AC-2.5: a blob over a real Unix-socket RPC
//! session, with and without file-descriptor passing.
//!
//! The unit tests next to `rsbinder::blob` write and read a blob in one
//! process, where the "shared region" never leaves it. Here the fd
//! crosses `SCM_RIGHTS` to another session and is mapped there — and the
//! same payload over a session with **no** fd mode has to arrive inline
//! instead, which is the degrade AOSP spells `!mAllowFds` and the reason
//! a blob needs no transport-specific handling.

#![cfg(all(
    feature = "rpc",
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]

use std::sync::Arc;
use std::thread;

use rsbinder::blob::BLOB_INPLACE_LIMIT;
use rsbinder::rpc::{FileDescriptorTransportMode as FdMode, RpcServer, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result as RsResult, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESCRIPTOR: &str = "rsbinder.test.blob.IBlob";
/// Reply: one blob of the length the request asks for.
const GET_BLOB: TransactionCode = FIRST_CALL_TRANSACTION;

fn pattern(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i % 251) as u8).collect()
}

struct BlobSvc;

impl Interface for BlobSvc {}

impl Remotable for BlobSvc {
    fn descriptor() -> &'static str {
        DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> RsResult<()> {
        match code {
            GET_BLOB => {
                let len: i32 = reader.read()?;
                // The service says nothing about the form: whether this
                // goes inline or through a region is decided by the
                // parcel it is writing into.
                reply.write_blob(&pattern(len as usize), false)
            }
            _ => Err(rsbinder::StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> RsResult<()> {
        Ok(())
    }
}

/// One bound server + a connector for it, as in `rpc_shared_memory.rs`:
/// Linux/macOS get a filesystem socket, Android an abstract name (the
/// `shell` domain cannot bind under `/data/local/tmp`).
struct Bound {
    server: Arc<RpcServer>,
    #[cfg(not(target_os = "android"))]
    path: std::path::PathBuf,
    #[cfg(target_os = "android")]
    name: Vec<u8>,
}

impl Bound {
    fn new(tag: &str) -> Self {
        let uniq = format!(
            "rsb_blob_{}_{}_{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        #[cfg(not(target_os = "android"))]
        {
            let mut path = std::env::temp_dir();
            path.push(format!("{uniq}.sock"));
            let server = RpcServer::setup_unix_server(&path).expect("bind");
            Self { server, path }
        }
        #[cfg(target_os = "android")]
        {
            let name = uniq.into_bytes();
            let server = RpcServer::setup_unix_server_abstract(&name).expect("bind abstract");
            server.set_android13plus(1);
            Self { server, name }
        }
    }

    fn run(&self) -> thread::JoinHandle<()> {
        let bg = self.server.run_background();
        #[cfg(not(target_os = "android"))]
        for _ in 0..400 {
            if self.path.exists() {
                break;
            }
            thread::sleep(std::time::Duration::from_millis(5));
        }
        bg
    }

    fn connect(&self, fd_mode: bool) -> RpcSession {
        #[cfg(not(target_os = "android"))]
        {
            let client = RpcSession::setup_unix_client(&self.path).expect("connect");
            if fd_mode {
                assert_eq!(
                    client.negotiate_fd_transport(FdMode::Unix).unwrap(),
                    FdMode::Unix
                );
            }
            client
        }
        #[cfg(target_os = "android")]
        {
            use rsbinder::rpc::RpcUnixClientConfig;
            let mut cfg = RpcUnixClientConfig::abstract_name(&self.name, 1);
            if fd_mode {
                cfg = cfg.fd_mode(FdMode::Unix);
            }
            RpcSession::setup_unix_client_android13plus_with_config(cfg).expect("connect abstract")
        }
    }

    fn finish(self, bg: thread::JoinHandle<()>) {
        self.server.stop_accepting();
        let _ = bg.join();
        #[cfg(not(target_os = "android"))]
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Ask for `len` bytes and return `(payload, went_inline)`.
fn fetch(session: &RpcSession, len: usize) -> (Vec<u8>, bool) {
    let root = session.get_root().expect("get_root");
    // A generated `from_binder` would do this; a hand-written client has
    // to stamp the descriptor itself, or `prepare_transact` writes an
    // interface token the server refuses with `BadType`.
    rsbinder::__rpc_stamp_descriptor(&root, DESCRIPTOR);
    let remote = root.as_remote().expect("an RPC root is a proxy");
    let mut data = remote.prepare_transact(true).expect("prepare_transact");
    data.write(&(len as i32)).expect("write len");
    let mut reply = remote
        .submit_transact(GET_BLOB, &data, 0)
        .expect("submit_transact")
        .expect("a twoway call has a reply");
    reply.set_data_position(0);
    let blob = reply.read_blob().expect("read_blob");
    assert_eq!(blob.len(), len);
    (blob.to_vec().expect("to_vec"), blob.inline().is_some())
}

fn serve(tag: &str, fd_mode: bool) -> (Bound, thread::JoinHandle<()>, RpcSession) {
    let bound = Bound::new(tag);
    if fd_mode {
        bound
            .server
            .set_supported_fd_modes(&[FdMode::None, FdMode::Unix]);
    }
    bound
        .server
        .set_root(Interface::as_binder(&Binder::new(BlobSvc)))
        .expect("set_root");
    let bg = bound.run();
    let session = bound.connect(fd_mode);
    (bound, bg, session)
}

/// AC-2.4: with fd passing negotiated, a payload over the limit arrives
/// as a shared region — the fd crossed `SCM_RIGHTS` and was mapped here.
#[test]
fn a_large_blob_crosses_a_unix_session_as_a_region() {
    let (bound, bg, session) = serve("fd", true);

    let len = BLOB_INPLACE_LIMIT + 4096;
    let (payload, inline) = fetch(&session, len);
    assert_eq!(payload, pattern(len));
    assert!(!inline, "with fd passing a large blob must go shared");

    // Under the limit it is inline whatever the transport can do.
    let small = BLOB_INPLACE_LIMIT - 1;
    let (payload, inline) = fetch(&session, small);
    assert_eq!(payload, pattern(small));
    assert!(inline, "a small blob is inline");

    session.close_session();
    bound.finish(bg);
}

/// AC-2.5, e2e half: the same payload over a session with no fd mode
/// arrives inline. Nothing in the service or the caller says so — the
/// parcel knows it cannot carry an fd, which is AOSP's `!mAllowFds`
/// branch.
#[test]
fn a_large_blob_falls_back_to_inline_without_fd_passing() {
    let (bound, bg, session) = serve("nofd", false);

    let len = BLOB_INPLACE_LIMIT + 4096;
    let (payload, inline) = fetch(&session, len);
    assert_eq!(payload, pattern(len));
    assert!(
        inline,
        "with no fd mode a large blob must degrade to inline"
    );

    session.close_session();
    bound.finish(bg);
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-3 AC-3.1, RPC half: 4 MB through a pipe whose read end
//! crossed a Unix-socket session.
//!
//! The parcel carries one file descriptor; the payload never enters it.
//! That is the point of the idiom, and it is also what makes the test
//! shape non-obvious: a pipe holds about 64 KB, so the writer blocks
//! until the peer drains it. Writer and reader are therefore on
//! different threads here — the same rule a service using this must
//! follow.

#![cfg(all(
    feature = "rpc",
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]

use std::io::{Read, Write};
use std::sync::Arc;
use std::thread;

use rsbinder::rpc::{FileDescriptorTransportMode as FdMode, RpcServer, RpcSession};
use rsbinder::{
    Binder, Interface, Parcel, ParcelFileDescriptor, Remotable, Result as RsResult,
    TransactionCode, FIRST_CALL_TRANSACTION,
};

const DESCRIPTOR: &str = "rsbinder.test.pipe.IPipe";
/// Request: int32 byte count. Reply: the read end of a pipe the service
/// is already filling.
const OPEN_STREAM: TransactionCode = FIRST_CALL_TRANSACTION;

const PAYLOAD: usize = 4 * 1024 * 1024;

/// Deterministic and cheap to check without keeping a copy.
fn byte_at(i: usize) -> u8 {
    (i % 251) as u8
}

struct PipeSvc;

impl Interface for PipeSvc {}

impl Remotable for PipeSvc {
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
            OPEN_STREAM => {
                let len: i32 = reader.read()?;
                let len = len as usize;
                let (read_end, write_end) = ParcelFileDescriptor::pipe()?;
                // Fill it from another thread: the pipe buffer is far
                // smaller than the payload, so writing here would block
                // this handler until the caller drained it — and the
                // caller has not even received the fd yet.
                thread::spawn(move || {
                    let mut written = 0usize;
                    while written < len {
                        let take = (64 * 1024).min(len - written);
                        // Generated from the absolute offset, not from a
                        // reused buffer: the pattern's period (251) does
                        // not divide the chunk size, so a reused chunk
                        // would not line up after the first one.
                        let piece: Vec<u8> = (written..written + take).map(byte_at).collect();
                        if (&write_end).write_all(&piece).is_err() {
                            return;
                        }
                        written += take;
                    }
                });
                reply.write(&read_end)
            }
            _ => Err(rsbinder::StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> RsResult<()> {
        Ok(())
    }
}

/// One bound server + a connector, as in `rpc_shared_memory.rs`.
struct Bound {
    server: Arc<RpcServer>,
    #[cfg(not(target_os = "android"))]
    path: std::path::PathBuf,
    #[cfg(target_os = "android")]
    name: Vec<u8>,
}

impl Bound {
    fn new() -> Self {
        let uniq = format!(
            "rsb_pipe_{}_{}",
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

    fn connect(&self) -> RpcSession {
        #[cfg(not(target_os = "android"))]
        {
            let client = RpcSession::setup_unix_client(&self.path).expect("connect");
            assert_eq!(
                client.negotiate_fd_transport(FdMode::Unix).unwrap(),
                FdMode::Unix
            );
            client
        }
        #[cfg(target_os = "android")]
        {
            use rsbinder::rpc::RpcUnixClientConfig;
            let cfg = RpcUnixClientConfig::abstract_name(&self.name, 1).fd_mode(FdMode::Unix);
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

#[test]
fn four_megabytes_cross_a_pipe_whose_fd_crossed_a_session() {
    let bound = Bound::new();
    bound
        .server
        .set_supported_fd_modes(&[FdMode::None, FdMode::Unix]);
    bound
        .server
        .set_root(Interface::as_binder(&Binder::new(PipeSvc)))
        .expect("set_root");
    let bg = bound.run();
    let session = bound.connect();

    {
        let root = session.get_root().expect("get_root");
        rsbinder::__rpc_stamp_descriptor(&root, DESCRIPTOR);
        let remote = root.as_remote().expect("an RPC root is a proxy");

        let mut data = remote.prepare_transact(true).expect("prepare_transact");
        data.write(&(PAYLOAD as i32)).expect("write len");
        let mut reply = remote
            .submit_transact(OPEN_STREAM, &data, 0)
            .expect("submit_transact")
            .expect("a twoway call has a reply");
        reply.set_data_position(0);
        let read_end: ParcelFileDescriptor = reply.read().expect("read the pipe fd");

        // Drain it here. The service is writing concurrently; neither
        // side ever holds 4 MB.
        let mut got = Vec::with_capacity(PAYLOAD);
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = (&read_end).read(&mut buf).expect("read from the pipe");
            if n == 0 {
                break;
            }
            got.extend_from_slice(&buf[..n]);
        }

        assert_eq!(got.len(), PAYLOAD, "the whole payload arrived");
        // Spot-check rather than build a 4 MB expectation: the pattern
        // is positional, so a shifted or duplicated chunk shows up here.
        for i in [0usize, 1, 250, 251, 65_535, 65_536, PAYLOAD - 1] {
            assert_eq!(got[i], byte_at(i), "byte {i}");
        }
    }

    session.close_session();
    bound.finish(bg);
}

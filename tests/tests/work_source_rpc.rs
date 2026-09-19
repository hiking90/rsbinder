// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-9 AC-9.4: the work source over an RPC session.
//!
//! The RPC wire format has no work-source field (AOSP's neither), so a
//! handler always reports the unset value, whatever the caller set. And
//! because the dispatch resets the thread's work source for each call, a
//! handler that sets one cannot leak it into the next call served by the
//! same thread. One `serve_blocking` thread serves every call below, which
//! is what makes the second claim observable.
//!
//! The kernel half is `tests/scripts/run_work_source_ac.sh`, which needs
//! separate processes.

#![cfg(feature = "rpc")]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.worksource.IRpcProbe";
/// Reply: `[work source uid as i32][should propagate as i32]` seen by the handler.
const GET: TransactionCode = FIRST_CALL_TRANSACTION;
/// Sets a propagating work source inside the handler, then replies like `GET`.
const SET: TransactionCode = FIRST_CALL_TRANSACTION + 1;

const UNSET: i32 = -1;

struct Probe;

impl Interface for Probe {}

impl Remotable for Probe {
    fn descriptor() -> &'static str {
        DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            GET => {}
            SET => {
                set_calling_work_source_uid(555);
            }
            _ => return Err(StatusCode::UnknownTransaction),
        }
        reply.write(&(get_calling_work_source_uid() as i32))?;
        reply.write(&(should_propagate_work_source() as i32))
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn call(remote: &dyn RemoteProxy, code: TransactionCode) -> (i32, bool) {
    // A hand-written client has no stamped descriptor; the RPC interface
    // token is just the descriptor string.
    let mut data = remote.prepare_transact(false).expect("prepare_transact");
    data.write(&DESCRIPTOR).expect("interface token");
    let mut reply = remote
        .submit_transact(code, &data, 0)
        .expect("submit_transact")
        .expect("two-way call has a reply");
    reply.set_data_position(0);
    let uid: i32 = reply.read().expect("uid");
    let propagate: i32 = reply.read().expect("propagate");
    (uid, propagate != 0)
}

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server
        .set_root(Interface::as_binder(&Binder::new(Probe)))
        .expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let root = client.get_root().expect("get_root");
        let remote = root.as_remote().expect("an RPC root is a proxy");

        assert_eq!(call(remote, GET), (UNSET, false));

        let token = set_calling_work_source_uid(1234);
        assert_eq!(
            call(remote, GET),
            (UNSET, false),
            "the RPC wire carries no work source, so the handler must not see the caller's"
        );
        restore_calling_work_source(token);

        assert_eq!(
            call(remote, SET),
            (555, true),
            "a handler can set a work source for its own outgoing kernel calls"
        );
        assert_eq!(
            call(remote, GET),
            (UNSET, false),
            "the previous handler's work source leaked into the next call on the same thread"
        );
    }

    handle.join().expect("server thread");
}

#[test]
fn work_source_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn work_source_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}

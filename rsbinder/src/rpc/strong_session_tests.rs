// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-17 Phase A: a proxy holds its session **strongly** (AOSP
//! `BpBinder` ↔ `sp<RpcSession>`), and session death releases every
//! local object the peer held (AOSP `RpcState::clear`) so the strong
//! ref cannot form a leak cycle through a service that stored a proxy
//! of its own session. Hermetic (Unix socketpair), in-crate so the
//! `Weak<RpcSessionInner>` leak probe is reachable.

use std::net::Shutdown;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use crate::rpc::transport::UnixTransport;
use crate::rpc::{AddressSpace, RpcProxy, RpcSession};
use crate::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IHolder";
const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_SET_CB: TransactionCode = FIRST_CALL_TRANSACTION + 1;

/// A service that stores whatever binder it is handed — the shape that
/// forms `session → local node → service → proxy → session`.
#[derive(Default)]
struct Holder {
    cb: Arc<Mutex<Option<SIBinder>>>,
}
impl Interface for Holder {}
impl Remotable for Holder {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            TX_ECHO => {
                let s: String = reader.read()?;
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&s)
            }
            TX_SET_CB => {
                let b: SIBinder = reader.read()?;
                *self.cb.lock().unwrap() = Some(b);
                reply.write(&Status::from(StatusCode::Ok))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}

fn echo(b: &SIBinder, s: &str) -> Result<String> {
    let rp = rpc_of(b);
    let mut d = rp.build_request(DESC)?;
    d.write(&s)?;
    let mut r = rp
        .transact(TX_ECHO, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    r.read::<String>()
}

fn set_cb(b: &SIBinder, cb: &SIBinder) -> Result<()> {
    let rp = rpc_of(b);
    let mut d = rp.build_request(DESC)?;
    d.write(cb)?;
    let mut r = rp
        .transact(TX_SET_CB, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if st.is_ok() {
        Ok(())
    } else {
        Err(StatusCode::from(st))
    }
}

/// (server session, client session, a dup of the server socket so the
/// test can kill the server end from outside — "server died").
fn pair_with_root(root: SIBinder) -> (RpcSession, RpcSession, UnixStream) {
    let (a, b) = UnixStream::pair().expect("socketpair");
    let server_dup = a.try_clone().expect("dup");
    let st = UnixTransport::from_stream(a).expect("transport");
    let ct = UnixTransport::from_stream(b).expect("transport");
    let server = RpcSession::new(Box::new(st), AddressSpace::Acceptor).expect("server");
    server.set_root(root);
    let client = RpcSession::new(Box::new(ct), AddressSpace::Initiator).expect("client");
    (server, client, server_dup)
}

fn wait_gone<T>(w: &Weak<T>) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if w.upgrade().is_none() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

/// D5: dropping the `RpcSession` handle does not kill proxies obtained
/// from it — the proxy alone keeps the connection; the connection ends
/// when the last proxy goes.
#[test]
fn proxy_outlives_session_handle() {
    let (server, client, _dup) =
        pair_with_root(Interface::as_binder(&Binder::new(Holder::default())));
    let s2 = server.clone();
    let jh = std::thread::spawn(move || s2.serve_blocking());
    let root = client.get_root().expect("root");
    drop(client);
    assert_eq!(
        echo(&root, "alive").expect("transact after handle drop"),
        "alive"
    );
    drop(root);
    // Last proxy gone ⇒ client transport closed ⇒ server serve loop ends.
    jh.join().expect("serve thread").ok();
    drop(server);
}

/// A.1 / test 2: the **server** stores a proxy to a client callback.
/// With the strong proxy→session ref that is `server inner → local
/// root → Holder → callback proxy → Arc<server inner>`. When the client
/// disconnects, the server's serve-loop exit must clear its local
/// objects so the whole graph is reclaimed.
#[test]
fn server_side_callback_cycle_reclaimed_on_client_disconnect() {
    let (server, client, _dup) =
        pair_with_root(Interface::as_binder(&Binder::new(Holder::default())));
    let probe = server.inner_weak();
    let s2 = server.clone();
    let jh = std::thread::spawn(move || s2.serve_blocking());
    drop(server);

    let root = client.get_root().expect("root");
    let cb: SIBinder = Interface::as_binder(&Binder::new(Holder::default()));
    set_cb(&root, &cb).expect("server stored our callback");
    drop(root);
    drop(client);
    drop(cb);

    jh.join().expect("serve thread").ok();
    assert!(
        wait_gone(&probe),
        "server session leaked through root → Holder → callback proxy"
    );
}

/// The cycle case the runtime cannot detect on its own: a client that
/// neither serves nor transacts again. `RpcSession::shutdown` is the
/// explicit break — without it the graph would stay alive for the
/// process lifetime.
#[test]
fn explicit_shutdown_breaks_cycle_without_any_transaction() {
    let (server, client, _dup) =
        pair_with_root(Interface::as_binder(&Binder::new(Holder::default())));
    let s2 = server.clone();
    let jh = std::thread::spawn(move || s2.serve_blocking());
    let probe = client.inner_weak();

    let root = client.get_root().expect("root");
    let slot = Arc::new(Mutex::new(Some(root.clone()))); // the cycle edge
    let cb: SIBinder = Interface::as_binder(&Binder::new(Holder {
        cb: Arc::clone(&slot),
    }));
    drop(slot);
    set_cb(&root, &cb).expect("server stored our callback");
    drop(cb);

    // Abandon the session: no serve loop here, no further transactions.
    drop(root);
    client.shutdown();
    drop(client);
    drop(server);

    // Asserted before the join: a regression leaves the client's transport
    // open, so the peer's serve loop would never see EOF and `join` would
    // hang instead of failing.
    assert!(
        wait_gone(&probe),
        "shutdown() must release the peer's local objects and break the cycle"
    );
    jh.join().expect("serve thread").ok();
}

/// A.1b / test 2b: the **client** has no serve thread and stores the
/// server root proxy inside a callback it handed to the server:
/// `client inner → local callback node → Holder → root proxy →
/// Arc<client inner>`. The server dies; the client's next call fails,
/// which must run the same death sequence (clear) so the client graph
/// is reclaimed once the user's own handles are gone.
#[test]
fn client_side_callback_cycle_reclaimed_on_server_death() {
    let (server, client, server_dup) =
        pair_with_root(Interface::as_binder(&Binder::new(Holder::default())));
    let s2 = server.clone();
    let jh = std::thread::spawn(move || s2.serve_blocking());
    let probe = client.inner_weak();

    let root = client.get_root().expect("root");
    let slot = Arc::new(Mutex::new(Some(root.clone()))); // the cycle edge
    let cb: SIBinder = Interface::as_binder(&Binder::new(Holder {
        cb: Arc::clone(&slot),
    }));
    drop(slot);
    set_cb(&root, &cb).expect("server stored our callback");
    // The server now holds a strong ref to `cb` (local node, strong=1).
    // No serve thread on the client: death is detected lazily.

    // Server dies.
    server_dup
        .shutdown(Shutdown::Both)
        .expect("kill server socket");
    jh.join().expect("serve thread").ok();
    drop(server);

    drop(cb);
    // Still reachable through the local node the server never DEC'd.
    assert!(
        probe.upgrade().is_some(),
        "pre-condition: cycle keeps inner alive"
    );
    assert!(echo(&root, "x").is_err(), "peer is gone");
    drop(root);
    drop(client);
    assert!(
        wait_gone(&probe),
        "client session leaked through callback → root proxy"
    );
}

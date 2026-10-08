// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC end-to-end: a hand-written AIDL-style interface driven
//! over the RPC stack with the **server stub reused unmodified** (the
//! generated free `on_transact` shape, dispatched via
//! `IBinder::rpc_transact` — never `Inner::transact`/`check_interface`)
//! and a **hand-written `RpcProxy` client**.
//!
//! Separate test binary (not a `src/` unit test) so it never shares a
//! process with the kernel-binder unit tests. Every
//! test builds its own session pair → parallel-safe, no `--test-threads=1`.
//!
//! # Notes
//!
//! - `rpc_call_via_generalized_remote_proxy_trait`: an RPC binder obtained from the stack is
//!   reachable through the **generalized** `dyn IBinder::as_remote()` as a
//!   `&dyn RemoteProxy`, and a full AIDL call driven via the trait's
//!   `prepare_transact`/`submit_transact` works over RPC — the same trait `ProxyHandle`
//!   implements for the kernel path. One abstraction serves both, with no generator change.
//! - `rpc_mode_parcel_rejects_file_descriptor`: an fd written into an RPC-mode parcel with no
//!   negotiated fd mode is a hard `FdsNotAllowed` reject, never a silent corruption or partial
//!   write. That is AOSP's answer for the same condition (`Parcel::writeFileDescriptor`,
//!   `FileDescriptorTransportMode::NONE`, android-15.0.0_r36 / android-16.0.0_r4).
//!
//! - Undecoded `ParcelableHolder` relay (`IRelay`): on android-16 v2 the copied binder takes its
//!   own reference (AOSP `Parcel::appendFrom`, android-16.0.0_r4) and a copied fd a slot of the
//!   reply's table; on r34, v0 and v1 no binder position is recorded, so the relay is refused
//!   with `BadType` as a status reply and the session keeps serving.
//!
//! Covers scalar/string/binder-arg e2e over `mem` *and*
//! `unix`, DEC_STRONG releasing the server node (no leak),
//! binder-in-parcel (reply binder → `RpcProxy` → re-call;
//! object-returning-home identity), and FD reject.

#![cfg(feature = "rpc")]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcProxy, RpcSession, RpcTransport};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, Status, StatusCode, TransactionCode,
    FIRST_CALL_TRANSACTION,
};

// ---- interface definitions (hand-written minimal fixture) -----------

const ISMOKE_DESC: &str = "rsbinder.test.ISmoke";
const ICHILD_DESC: &str = "rsbinder.test.IChild";

const TX_ECHO: TransactionCode = FIRST_CALL_TRANSACTION;
const TX_ADD: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const TX_GET_CHILD: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const TX_PASS_BINDER: TransactionCode = FIRST_CALL_TRANSACTION + 3;
/// oneway; the handler writes the child into a reply nobody sends.
const TX_ONEWAY_CHILD: TransactionCode = FIRST_CALL_TRANSACTION + 4;
const TX_CHILD_NAME: TransactionCode = FIRST_CALL_TRANSACTION;

trait ISmoke: Interface {
    fn echo(&self, s: &str) -> Result<String>;
    fn add(&self, a: i32, b: i32) -> Result<i32>;
    fn get_child(&self) -> Result<SIBinder>;
    /// Returns the passed binder's descriptor (a binder *argument* + the returning-home path).
    fn pass_binder(&self, b: &SIBinder) -> Result<String>;
}

trait IChild: Interface {
    fn name(&self) -> Result<String>;
}

// ---- server impls ---------------------------------------------------

struct ChildSvc {
    name: String,
}
impl Interface for ChildSvc {}
impl IChild for ChildSvc {
    fn name(&self) -> Result<String> {
        Ok(self.name.clone())
    }
}

struct SmokeSvc {
    child: SIBinder,
}
impl Interface for SmokeSvc {}
impl ISmoke for SmokeSvc {
    fn echo(&self, s: &str) -> Result<String> {
        Ok(s.to_string())
    }
    fn add(&self, a: i32, b: i32) -> Result<i32> {
        Ok(a + b)
    }
    fn get_child(&self) -> Result<SIBinder> {
        Ok(self.child.clone())
    }
    fn pass_binder(&self, b: &SIBinder) -> Result<String> {
        // Returned from the client, it must resolve to *our* original local child.
        Ok(b.descriptor().to_string())
    }
}

// Generator-shaped `on_transact`: RPC reaches it via `rpc_transact`, never `check_interface`.

fn smoke_on_transact(
    s: &dyn ISmoke,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_ECHO => {
            let arg: String = reader.read()?;
            let r = s.echo(&arg);
            write_result_string(reply, r)
        }
        TX_ADD => {
            let a: i32 = reader.read()?;
            let b: i32 = reader.read()?;
            match s.add(a, b) {
                Ok(v) => {
                    reply.write(&Status::from(StatusCode::Ok))?;
                    reply.write(&v)
                }
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        TX_GET_CHILD => match s.get_child() {
            Ok(b) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&b)
            }
            Err(e) => reply.write(&Status::from(e)),
        },
        TX_PASS_BINDER => {
            let b: SIBinder = reader.read()?;
            write_result_string(reply, s.pass_binder(&b))
        }
        TX_ONEWAY_CHILD => {
            reply.write(&Status::from(StatusCode::Ok))?;
            reply.write(&s.get_child()?)
        }
        _ => Err(StatusCode::UnknownTransaction),
    }
}

fn child_on_transact(
    s: &dyn IChild,
    code: TransactionCode,
    _reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_CHILD_NAME => write_result_string(reply, s.name()),
        _ => Err(StatusCode::UnknownTransaction),
    }
}

fn write_result_string(reply: &mut Parcel, r: Result<String>) -> Result<()> {
    match r {
        Ok(v) => {
            reply.write(&Status::from(StatusCode::Ok))?;
            reply.write(&v)
        }
        Err(e) => reply.write(&Status::from(e)),
    }
}

// ---- Bn wrappers (Remotable; new_binder shape) ----------------------

struct BnSmoke(Box<dyn ISmoke + Send + Sync>);
impl Remotable for BnSmoke {
    fn descriptor() -> &'static str {
        ISMOKE_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        smoke_on_transact(&*self.0, code, reader, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

struct BnChild(Box<dyn IChild + Send + Sync>);
impl Remotable for BnChild {
    fn descriptor() -> &'static str {
        ICHILD_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        child_on_transact(&*self.0, code, reader, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

// ---- hand-written client proxies (drive RpcProxy directly) ----------

fn rpc_of(b: &SIBinder) -> &RpcProxy {
    (**b)
        .as_any()
        .downcast_ref::<RpcProxy>()
        .expect("client binder must be an RpcProxy")
}

fn read_status(reply: &mut Parcel) -> Result<()> {
    let st: Status = reply.read()?;
    if st.is_ok() {
        Ok(())
    } else {
        Err(StatusCode::from(st))
    }
}

struct SmokeProxy(SIBinder);
impl SmokeProxy {
    fn echo(&self, s: &str) -> Result<String> {
        let rp = rpc_of(&self.0);
        let mut d = rp.build_request(ISMOKE_DESC)?;
        d.write(&s)?;
        let mut r = rp
            .transact(TX_ECHO, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
    fn add(&self, a: i32, b: i32) -> Result<i32> {
        let rp = rpc_of(&self.0);
        let mut d = rp.build_request(ISMOKE_DESC)?;
        d.write(&a)?;
        d.write(&b)?;
        let mut r = rp
            .transact(TX_ADD, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<i32>()
    }
    fn get_child(&self) -> Result<SIBinder> {
        let rp = rpc_of(&self.0);
        let d = rp.build_request(ISMOKE_DESC)?;
        let mut r = rp
            .transact(TX_GET_CHILD, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<SIBinder>()
    }
    fn pass_binder(&self, b: &SIBinder) -> Result<String> {
        let rp = rpc_of(&self.0);
        let mut d = rp.build_request(ISMOKE_DESC)?;
        d.write(b)?;
        let mut r = rp
            .transact(TX_PASS_BINDER, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
}

struct ChildProxy(SIBinder);
impl ChildProxy {
    fn name(&self) -> Result<String> {
        let rp = rpc_of(&self.0);
        let d = rp.build_request(ICHILD_DESC)?;
        let mut r = rp
            .transact(TX_CHILD_NAME, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        r.read::<String>()
    }
}

// ---- harness --------------------------------------------------------

fn make_root() -> SIBinder {
    let child = Interface::as_binder(&Binder::new(BnChild(Box::new(ChildSvc {
        name: "child-1".to_string(),
    }))));
    Interface::as_binder(&Binder::new(BnSmoke(Box::new(SmokeSvc { child }))))
}

/// Runs the full scenario over a connected transport pair, asserting server node accounting.
fn run_scenario(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root()).expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let root = SmokeProxy(client.get_root().expect("get_root"));

        // Scalar + string round-trip, exact values.
        assert_eq!(root.echo("hello rpc").unwrap(), "hello rpc");
        assert_eq!(root.echo("").unwrap(), "");
        assert_eq!(root.add(2, 3).unwrap(), 5);
        assert_eq!(root.add(-7, 7).unwrap(), 0);

        // Reply contains a binder → client builds an RpcProxy → re-calls it.
        let child_sib = root.get_child().unwrap();
        assert!(
            (*child_sib).as_any().downcast_ref::<RpcProxy>().is_some(),
            "AC-2.6: a binder in an RPC reply must become an RpcProxy"
        );
        let child = ChildProxy(child_sib);
        assert_eq!(child.name().unwrap(), "child-1");

        // Binder *argument*: the server recognises the proxy as its own local object.
        assert_eq!(root.pass_binder(&child.0).unwrap(), ICHILD_DESC);

        // DEC_STRONG on drop; the next ordered round-trip proves the server processed it.
        assert_eq!(server.local_node_count(), 2, "root + child registered");
        drop(child);
        assert_eq!(root.echo("flush").unwrap(), "flush");
        assert_eq!(
            server.local_node_count(),
            1,
            "AC-2.5: child node released after DEC_STRONG (no leak)"
        );
    }

    handle.join().expect("server thread");
}

#[test]
fn rpc_e2e_over_mem() {
    let (a, b) = MemTransport::pair();
    run_scenario(Box::new(a), Box::new(b));
}

#[test]
fn rpc_e2e_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run_scenario(Box::new(a), Box::new(b));
}

/// An RPC binder's `as_remote()` drives a full call via `RemoteProxy`; see module doc "Notes".
#[test]
fn rpc_call_via_generalized_remote_proxy_trait() {
    use rsbinder::RemoteProxy;

    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root()).expect("set_root");
    let h = thread::spawn(move || {
        let _ = server.serve_blocking();
    });

    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root");
    rsbinder::__rpc_stamp_descriptor(&root, ISMOKE_DESC);

    // `as_proxy()` is kernel-only; `as_remote()` also resolves an RPC binder.
    let remote = (*root)
        .as_remote()
        .expect("AC-6: an RpcProxy must be reachable as &dyn RemoteProxy");

    // The trait's parcel carries the interface token the server checks.
    let mut d = remote.prepare_transact(true).expect("prepare_transact");
    d.write(&"via-remote-proxy").unwrap();
    let mut reply = RemoteProxy::submit_transact(remote, TX_ECHO, &d, 0)
        .expect("submit_transact via &dyn RemoteProxy")
        .expect("reply");
    read_status(&mut reply).unwrap();
    assert_eq!(reply.read::<String>().unwrap(), "via-remote-proxy");

    drop(root);
    drop(client);
    h.join().unwrap();
}

/// A binder read after the session closed is `DeadObject` (AOSP `RpcState::onBinderEntering`).
#[test]
fn binder_read_after_session_close_is_dead_object() {
    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root()).expect("set_root");
    let h = thread::spawn(move || {
        let _ = server.serve_blocking();
    });

    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root");
    let rp = rpc_of(&root);
    let d = rp.build_request(ISMOKE_DESC).unwrap();
    let mut reply = rp
        .transact(TX_GET_CHILD, &d, 0)
        .expect("get_child")
        .expect("reply");
    read_status(&mut reply).unwrap();
    client.close_session();
    assert_eq!(reply.read::<SIBinder>().err(), Some(StatusCode::DeadObject));

    drop(reply);
    drop(root);
    drop(client);
    h.join().unwrap();
}

/// Up to two seconds for `f`; the last evaluation is the verdict.
fn poll_until(mut f: impl FnMut() -> bool) -> bool {
    for _ in 0..400 {
        if f() {
            return true;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    f()
}

/// A served `mem` pair: the server (with its serve thread) and a client session holding the root.
struct MemPair {
    server: RpcSession,
    client: RpcSession,
    root: SIBinder,
    serve: Option<thread::JoinHandle<()>>,
}
impl MemPair {
    fn new() -> Self {
        let (a, b) = MemTransport::pair();
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
        server.set_root(make_root()).expect("set_root");
        let server_for_thread = server.clone();
        let serve = thread::spawn(move || {
            let _ = server_for_thread.serve_blocking();
        });
        let client =
            RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
        let root = client.get_root().expect("get_root");
        Self {
            server,
            client,
            root,
            serve: Some(serve),
        }
    }
    /// Ends the client first, so the serve thread returns before this joins it.
    fn finish(mut self) {
        self.client.close_session();
        self.serve.take().expect("serve thread").join().unwrap();
    }
    /// Joins the serve thread of a session already ended from the server side.
    fn serve_thread_join(mut self) {
        self.serve.take().expect("serve thread").join().unwrap();
    }
}

/// `is_ended` reads the session's state: set by an end on either side once this end sees it.
#[test]
fn is_ended_follows_the_session_not_the_peer() {
    let p = MemPair::new();
    assert!(!p.client.is_ended() && !p.server.is_ended());

    // The peer ends; this client has no serve loop, so it learns only from its next call.
    p.server.close_session();
    assert!(p.server.is_ended());
    assert!(!p.client.is_ended(), "no I/O since the peer closed");
    let rp = rpc_of(&p.root);
    let d = rp.build_request(ISMOKE_DESC).unwrap();
    assert!(rp.transact(TX_ECHO, &d, 0).is_err());
    assert!(
        p.client.is_ended(),
        "the failed call ended the client's session"
    );

    p.serve_thread_join();
}

/// A local binder written after `close_session` is refused at the write; nothing is inserted.
#[test]
fn write_after_close_session_is_refused() {
    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root()).expect("set_root");
    let h = thread::spawn(move || {
        let _ = server.serve_blocking();
    });

    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root");
    client.close_session();
    let rp = rpc_of(&root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
    assert_eq!(
        d.write(&make_root()),
        Err(StatusCode::DeadObject),
        "AOSP `onBinderLeaving`: a torn-down session takes no binder"
    );
    assert_eq!(
        client.local_node_count(),
        0,
        "the refused write inserted a node that no `clear_local` will ever drain"
    );
    assert!(rp.transact(TX_PASS_BINDER, &d, 0).is_err());
    assert_eq!(client.local_node_count(), 0);

    drop(d);
    assert_eq!(client.local_node_count(), 0);
    drop(root);
    drop(client);
    h.join().unwrap();
}

/// A request written and never sent gives its argument's bump back when it drops.
#[test]
fn an_unsent_request_releases_its_node_on_drop() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
    d.write(&make_root()).unwrap();
    assert_eq!(p.client.local_node_count(), 1, "the argument took its bump");
    drop(d);
    assert_eq!(
        p.client.local_node_count(),
        0,
        "never sent: the parcel's drop gives the bump back"
    );
    p.finish();
}

/// A parcel is sent once (a resend is `InvalidOperation`); the peer's `DEC_STRONG` frees the node.
#[test]
fn a_sent_parcel_is_refused_on_resend() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
    d.write(&make_root()).unwrap();
    let mut r = rp.transact(TX_PASS_BINDER, &d, 0).unwrap().unwrap();
    read_status(&mut r).unwrap();
    // A fresh server-side proxy has no descriptor yet; the reply's shape is what matters here.
    let _ = r.read::<String>().unwrap();
    assert_eq!(
        rp.transact(TX_PASS_BINDER, &d, 0).err(),
        Some(StatusCode::InvalidOperation),
        "a sent parcel is refused on resend"
    );
    // `pass_binder` keeps no proxy: its DEC_STRONG releases the node; a second send would not.
    assert!(
        poll_until(|| p.client.local_node_count() == 0),
        "the argument's node is released once by the peer's DEC_STRONG"
    );
    drop(d);
    p.finish();
}

/// AOSP `RpcState::validateParcel`: a parcel built on one session is `BadType` on another.
#[test]
fn a_parcel_of_another_session_is_refused() {
    let a = MemPair::new();
    let b = MemPair::new();
    let mut d = rpc_of(&a.root).build_request(ISMOKE_DESC).unwrap();
    d.write(&make_root()).unwrap();
    assert_eq!(
        rpc_of(&b.root).transact(TX_PASS_BINDER, &d, 0).err(),
        Some(StatusCode::BadType),
        "B's peer never learns A's address, so no DEC_STRONG would ever settle A's bump"
    );
    assert_eq!(a.client.local_node_count(), 1, "the refusal left d unsent");
    drop(d);
    assert_eq!(
        a.client.local_node_count(),
        0,
        "unsent: the drop settles on A"
    );
    b.finish();
    a.finish();
}

/// A sent parcel takes no further local binder: nothing would settle the bump.
#[test]
fn a_local_binder_written_after_the_send_is_refused() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
    d.write(&"x").unwrap();
    rp.transact(TX_ECHO, &d, 0).unwrap();
    assert_eq!(d.write(&make_root()), Err(StatusCode::BadType));
    assert_eq!(
        p.client.local_node_count(),
        0,
        "the refused write took no bump"
    );
    p.finish();
}

/// A reply is received bytes (AOSP `RECEIVED`): handing it back as a request is refused.
#[test]
fn a_received_reply_is_not_sent_again() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
    d.write(&"x").unwrap();
    let r = rp.transact(TX_ECHO, &d, 0).unwrap().unwrap();
    assert_eq!(
        rp.transact(TX_ECHO, &r, 0).err(),
        Some(StatusCode::InvalidOperation)
    );
    p.finish();
}

/// The reply parcel that carried a binder drops after the send without taking the node with it.
#[test]
fn a_returned_binder_survives_the_reply_parcel_drop() {
    let p = MemPair::new();
    let root = SmokeProxy(p.root.clone());
    let child = ChildProxy(root.get_child().unwrap());
    assert_eq!(
        child.name().unwrap(),
        "child-1",
        "the binder a sent reply carried stays registered"
    );
    assert_eq!(p.server.local_node_count(), 2, "root + child");
    drop(child);
    assert_eq!(root.echo("flush").unwrap(), "flush");
    assert_eq!(
        p.server.local_node_count(),
        1,
        "child released by DEC_STRONG"
    );
    drop(root);
    p.finish();
}

/// A oneway handler's reply is never sent: the binder it wrote is released when the reply drops.
#[test]
fn a_oneway_handler_reply_binder_is_released() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let d = rp.build_request(ISMOKE_DESC).unwrap();
    assert!(rp
        .transact(TX_ONEWAY_CHILD, &d, rsbinder::FLAG_ONEWAY)
        .unwrap()
        .is_none());
    // Same slot, in order: once the echo returns, the oneway handler has run and its reply dropped.
    assert_eq!(SmokeProxy(p.root.clone()).echo("flush").unwrap(), "flush");
    assert_eq!(
        p.server.local_node_count(),
        1,
        "root only: the child written into the unsent oneway reply was released"
    );
    p.finish();
}

/// No fd mode: an fd write into an RPC parcel is `FdsNotAllowed`, as in AOSP; see module doc.
#[test]
fn rpc_mode_parcel_rejects_file_descriptor() {
    use rsbinder::ParcelFileDescriptor;
    use std::fs::File;

    let mut p = Parcel::new();
    p.__set_for_rpc(true).unwrap();
    let pfd = ParcelFileDescriptor::new(File::open("/dev/null").expect("/dev/null"));
    let err = p
        .write(&pfd)
        .expect_err("FD in RPC parcel must be rejected");
    assert_eq!(
        err,
        StatusCode::FdsNotAllowed,
        "AOSP FDS_NOT_ALLOWED for a session that negotiated no fd mode"
    );

    // Kernel-mode parcel still accepts an FD (no regression).
    let mut k = Parcel::new();
    assert!(k.is_kernel_backed());
    let pfd2 = ParcelFileDescriptor::new(File::open("/dev/null").expect("/dev/null"));
    k.write(&pfd2).expect("kernel-mode FD write still works");
}

/// AOSP `RpcState::validateParcel`: a parcel with no session is `BadType`, not sent as is.
#[test]
fn rpc_transact_refuses_a_parcel_built_for_no_session() {
    let p = MemPair::new();
    let rp = rpc_of(&p.root);
    let mut data_only = Parcel::new();
    data_only.__set_for_rpc(true).unwrap();
    for mut d in [Parcel::new(), data_only] {
        d.write(&ISMOKE_DESC).unwrap();
        d.write(&"x").unwrap();
        assert_eq!(rp.transact(TX_ECHO, &d, 0).err(), Some(StatusCode::BadType));
    }
    assert_eq!(SmokeProxy(p.root.clone()).echo("ok").unwrap(), "ok");
    p.finish();
}

// ---- undecoded ParcelableHolder relay (module doc) ------------------

use rsbinder::rpc::FileDescriptorTransportMode as FdMode;
use rsbinder::{ParcelFileDescriptor, Parcelable, ParcelableHolder, ParcelableMetadata};

const IRELAY_DESC: &str = "rsbinder.test.IRelay";
const TX_RELAY_CHILD: TransactionCode = FIRST_CALL_TRANSACTION;
/// Writes the received holder into the reply undecoded: `append_from` copies its bytes.
const TX_RELAY_ECHO: TransactionCode = FIRST_CALL_TRANSACTION + 1;
/// The same after an fd of the server's own, so the copied fd is the reply's second.
const TX_RELAY_ECHO_AFTER_FD: TransactionCode = FIRST_CALL_TRANSACTION + 2;
/// The same, then fails: the reply holding the copy drops unsent.
const TX_RELAY_ECHO_THEN_FAIL: TransactionCode = FIRST_CALL_TRANSACTION + 3;
/// Decodes the holder: `"<binder descriptor>|<fd content>|<value>"`.
const TX_RELAY_DECODE: TransactionCode = FIRST_CALL_TRANSACTION + 4;
/// Swaps the received request in as the reply, which the send refuses.
const TX_RELAY_REPLY_WITH_REQUEST: TransactionCode = FIRST_CALL_TRANSACTION + 5;
/// A reply past `MAX_FRAME_LEN`: the encode refuses it before any byte.
const TX_RELAY_REPLY_TOO_LARGE: TransactionCode = FIRST_CALL_TRANSACTION + 6;
/// A reply with more fds than one frame carries.
const TX_RELAY_REPLY_TOO_MANY_FDS: TransactionCode = FIRST_CALL_TRANSACTION + 7;
/// Replaces the reply with a parcel built for no session.
const TX_RELAY_REPLY_WITHOUT_SESSION: TransactionCode = FIRST_CALL_TRANSACTION + 8;

/// A holder payload with a binder and an fd slot.
#[derive(Debug, Default)]
struct Carrier {
    value: i32,
    binder: Option<SIBinder>,
    fd: Option<ParcelFileDescriptor>,
}
impl ParcelableMetadata for Carrier {
    fn descriptor() -> &'static str {
        "rsbinder.test.Carrier"
    }
}
impl Parcelable for Carrier {
    fn write_to_parcel(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.value)?;
        parcel.write(&self.binder)?;
        parcel.write(&self.fd)
    }
    fn read_from_parcel(&mut self, parcel: &mut Parcel) -> Result<()> {
        self.value = parcel.read()?;
        self.binder = parcel.read()?;
        self.fd = parcel.read()?;
        Ok(())
    }
}

fn holder_of(carrier: Carrier) -> ParcelableHolder {
    let mut h = ParcelableHolder::default();
    h.set_parcelable(std::sync::Arc::new(carrier)).unwrap();
    h
}

/// An unlinked temporary file holding `bytes`.
fn pfd_with(bytes: &[u8]) -> ParcelFileDescriptor {
    use std::io::Write;
    use std::sync::atomic::{AtomicU32, Ordering};
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let mut path = std::env::temp_dir();
    path.push(format!(
        "rsb_e2e_relay_{}_{}.tmp",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let mut f = std::fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(&path)
        .expect("tempfile");
    let _ = std::fs::remove_file(&path);
    f.write_all(bytes).unwrap();
    ParcelFileDescriptor::new(f)
}

/// The file's bytes from offset 0, leaving the shared offset alone.
fn content(pfd: &ParcelFileDescriptor) -> String {
    use std::os::unix::fs::FileExt;
    let f = std::fs::File::from(pfd.as_ref().try_clone().expect("dup"));
    let mut buf = [0u8; 64];
    let n = f.read_at(&mut buf, 0).expect("pread");
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

struct BnRelay {
    child: SIBinder,
}
impl Remotable for BnRelay {
    fn descriptor() -> &'static str {
        IRELAY_DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        let ok = Status::from(StatusCode::Ok);
        match code {
            TX_RELAY_CHILD => {
                reply.write(&ok)?;
                reply.write(&self.child)
            }
            TX_RELAY_ECHO => {
                let h: ParcelableHolder = reader.read()?;
                reply.write(&ok)?;
                reply.write(&h)
            }
            TX_RELAY_ECHO_AFTER_FD => {
                let h: ParcelableHolder = reader.read()?;
                reply.write(&ok)?;
                reply.write(&pfd_with(b"server-fd"))?;
                reply.write(&h)
            }
            TX_RELAY_ECHO_THEN_FAIL => {
                let h: ParcelableHolder = reader.read()?;
                reply.write(&ok)?;
                reply.write(&h)?;
                Err(StatusCode::PermissionDenied)
            }
            TX_RELAY_DECODE => {
                let h: ParcelableHolder = reader.read()?;
                let c = h.get_parcelable::<Carrier>()?.ok_or(StatusCode::BadValue)?;
                let desc = c.binder.as_ref().map(|b| b.descriptor().to_string());
                let fd = c.fd.as_ref().map(content);
                reply.write(&ok)?;
                reply.write(&format!(
                    "{}|{}|{}",
                    desc.unwrap_or_default(),
                    fd.unwrap_or_default(),
                    c.value
                ))
            }
            TX_RELAY_REPLY_WITH_REQUEST => {
                std::mem::swap(reader, reply);
                Ok(())
            }
            TX_RELAY_REPLY_TOO_LARGE => {
                reply.write(&ok)?;
                reply.write(&vec![0u8; rsbinder::rpc::transport::MAX_FRAME_LEN][..])
            }
            TX_RELAY_REPLY_TOO_MANY_FDS => {
                reply.write(&ok)?;
                // One past the transport's per-frame fd cap (64).
                for _ in 0..65 {
                    let null = std::fs::File::open("/dev/null").map_err(|_| StatusCode::BadFd)?;
                    reply.write(&ParcelFileDescriptor::new(null))?;
                }
                Ok(())
            }
            TX_RELAY_REPLY_WITHOUT_SESSION => {
                *reply = Parcel::new();
                reply.write(&ok)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

/// The wire a [`RelayPair`] speaks.
#[derive(Clone, Copy, Debug)]
enum Wire {
    R34,
    /// android-13+ at this `RPC_WIRE_PROTOCOL_VERSION`, fds over `SCM_RIGHTS` from v1.
    A13(u32),
}

/// A served `unix` socketpair on `Wire` whose server root is [`BnRelay`].
struct RelayPair {
    server: RpcSession,
    client: RpcSession,
    root: SIBinder,
    serve: Option<thread::JoinHandle<()>>,
}
impl RelayPair {
    fn new(wire: Wire) -> Self {
        let (a, b) = UnixTransport::pair().expect("socketpair");
        let (server, client) = match wire {
            Wire::R34 => (
                RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("server"),
                RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("client"),
            ),
            Wire::A13(v) => {
                let accept = thread::spawn(move || {
                    RpcSession::accept_android13plus_fd(Box::new(a), v, true).expect("accept")
                });
                let client = RpcSession::connect_android13plus_fd(Box::new(b), v, FdMode::Unix)
                    .expect("connect");
                (accept.join().expect("accept thread"), client)
            }
        };
        let child = Interface::as_binder(&Binder::new(BnChild(Box::new(ChildSvc {
            name: "child-1".to_string(),
        }))));
        server
            .set_root(Interface::as_binder(&Binder::new(BnRelay { child })))
            .expect("set_root");
        let server_for_thread = server.clone();
        let serve = thread::spawn(move || {
            let _ = server_for_thread.serve_blocking();
        });
        let root = client.get_root().expect("get_root");
        Self {
            server,
            client,
            root,
            serve: Some(serve),
        }
    }

    fn child(&self) -> SIBinder {
        let rp = rpc_of(&self.root);
        let d = rp.build_request(IRELAY_DESC).unwrap();
        let mut r = rp.transact(TX_RELAY_CHILD, &d, 0).unwrap().unwrap();
        read_status(&mut r).unwrap();
        r.read().unwrap()
    }

    /// Sends `holder` to `code`; the reply after its status.
    fn call(&self, code: TransactionCode, holder: &ParcelableHolder) -> Result<Parcel> {
        let rp = rpc_of(&self.root);
        let mut d = rp.build_request(IRELAY_DESC)?;
        d.write(holder)?;
        let mut r = rp
            .transact(code, &d, 0)?
            .ok_or(StatusCode::UnexpectedNull)?;
        read_status(&mut r)?;
        Ok(r)
    }

    fn finish(mut self) {
        drop(self.root);
        self.client.close_session();
        self.serve.take().expect("serve thread").join().unwrap();
    }
}

/// v2: the relayed copy takes its own reference, so the excess `DEC_STRONG` leaves the node alive.
#[test]
fn a_v2_holder_relay_takes_its_own_reference_on_a_local_binder() {
    let p = RelayPair::new(Wire::A13(2));
    assert_eq!(p.server.local_node_count(), 1, "root");
    let child = p.child();
    assert_eq!(p.server.local_node_count(), 2, "root + child");

    let carrier = Carrier {
        value: 7,
        binder: Some(child.clone()),
        fd: None,
    };
    let mut r = p.call(TX_RELAY_ECHO, &holder_of(carrier)).unwrap();
    let echoed: ParcelableHolder = r.read().unwrap();
    let c = echoed.get_parcelable::<Carrier>().unwrap().unwrap();
    assert_eq!(c.value, 7);
    let relayed = ChildProxy(c.binder.clone().expect("the relayed binder"));
    assert_eq!(relayed.name().unwrap(), "child-1");
    drop(relayed);
    // The duplicate receipt's DEC_STRONG went out on this connection before the ping.
    assert_eq!(
        child.ping_binder(),
        Ok(()),
        "the node outlived the excess DEC_STRONG"
    );
    assert_eq!(p.server.local_node_count(), 2);

    drop((c, echoed, r, child));
    assert!(
        poll_until(|| p.server.local_node_count() == 1),
        "every reference the relay took was settled"
    );
    p.finish();
}

/// v2: a reply that holds a relayed copy and is never sent gives the copy's reference back.
#[test]
fn a_v2_relay_into_a_discarded_reply_gives_its_reference_back() {
    let p = RelayPair::new(Wire::A13(2));
    let child = p.child();
    let carrier = Carrier {
        value: 1,
        binder: Some(child.clone()),
        fd: None,
    };
    assert_eq!(
        p.call(TX_RELAY_ECHO_THEN_FAIL, &holder_of(carrier)).err(),
        Some(StatusCode::PermissionDenied)
    );
    assert_eq!(child.ping_binder(), Ok(()));
    assert_eq!(p.server.local_node_count(), 2, "root + child");
    drop(child);
    assert!(
        poll_until(|| p.server.local_node_count() == 1),
        "the discarded reply's reference was given back"
    );
    p.finish();
}

/// r34, v0 and v1 record no binder position, so no copy can be proven binder-free: refused.
#[test]
fn a_holder_relay_is_refused_without_binder_positions() {
    for wire in [Wire::R34, Wire::A13(0), Wire::A13(1)] {
        let p = RelayPair::new(wire);
        let child = p.child();
        let carriers = [
            Carrier {
                value: 1,
                binder: Some(child.clone()),
                fd: None,
            },
            Carrier {
                value: 2,
                binder: None,
                fd: None,
            },
        ];
        for carrier in carriers {
            assert_eq!(
                p.call(TX_RELAY_ECHO, &holder_of(carrier)).err(),
                Some(StatusCode::BadType),
                "{wire:?}: the refusal reaches the caller as a status"
            );
        }
        assert_eq!(
            p.root.ping_binder(),
            Ok(()),
            "{wire:?}: the session still serves"
        );
        assert_eq!(child.ping_binder(), Ok(()), "{wire:?}");
        assert_eq!(p.server.local_node_count(), 2, "{wire:?}: root + child");
        drop(child);
        assert!(
            poll_until(|| p.server.local_node_count() == 1),
            "{wire:?}: the books balance"
        );
        p.finish();
    }
}

/// v1 and v2: the holder's sub-parcel keeps its objects, so the handler decodes both.
#[test]
fn a_holder_resolves_its_binder_and_fd_where_the_wire_records_them() {
    for v in [1, 2] {
        let p = RelayPair::new(Wire::A13(v));
        let child = p.child();
        let carrier = Carrier {
            value: 5,
            binder: Some(child.clone()),
            fd: Some(pfd_with(b"client-fd")),
        };
        let mut r = p.call(TX_RELAY_DECODE, &holder_of(carrier)).unwrap();
        assert_eq!(
            r.read::<String>().unwrap(),
            format!("{ICHILD_DESC}|client-fd|5"),
            "v{v}"
        );
        drop((r, child));
        assert!(poll_until(|| p.server.local_node_count() == 1), "v{v}");
        p.finish();
    }
}

/// v2: a holder with no binder relays, and its fd takes the next slot of the reply's table.
#[test]
fn a_v2_holder_relays_data_and_fds() {
    let p = RelayPair::new(Wire::A13(2));
    let carrier = Carrier {
        value: 42,
        binder: None,
        fd: None,
    };
    let mut r = p.call(TX_RELAY_ECHO, &holder_of(carrier)).unwrap();
    let h: ParcelableHolder = r.read().unwrap();
    assert_eq!(h.get_parcelable::<Carrier>().unwrap().unwrap().value, 42);

    let carrier = Carrier {
        value: 43,
        binder: None,
        fd: Some(pfd_with(b"client-fd")),
    };
    let mut r = p.call(TX_RELAY_ECHO_AFTER_FD, &holder_of(carrier)).unwrap();
    let server_fd: ParcelFileDescriptor = r.read().unwrap();
    let h: ParcelableHolder = r.read().unwrap();
    let c = h.get_parcelable::<Carrier>().unwrap().unwrap();
    assert_eq!(content(&server_fd), "server-fd");
    assert_eq!(c.value, 43);
    assert_eq!(c.fd.as_ref().map(content).as_deref(), Some("client-fd"));
    p.finish();
}

/// AOSP `processTransactInternal`: a reply the send refuses goes back as that status.
#[test]
fn a_reply_the_send_refuses_goes_back_as_a_status() {
    let p = RelayPair::new(Wire::R34);
    let rp = rpc_of(&p.root);
    let d = rp.build_request(IRELAY_DESC).unwrap();
    assert_eq!(
        rp.transact(TX_RELAY_REPLY_WITH_REQUEST, &d, 0).err(),
        Some(StatusCode::InvalidOperation),
        "a received parcel is not sent back as the reply"
    );
    assert_eq!(p.root.ping_binder(), Ok(()), "the connection still serves");
    p.finish();
}

/// A reply the frame cannot carry goes back as a status, and the session keeps serving.
#[test]
fn a_reply_refused_before_its_first_byte_goes_back_as_a_status() {
    let cases = [
        (
            Wire::R34,
            TX_RELAY_REPLY_TOO_LARGE,
            StatusCode::FailedTransaction,
        ),
        (
            Wire::A13(2),
            TX_RELAY_REPLY_TOO_LARGE,
            StatusCode::FailedTransaction,
        ),
        (
            Wire::A13(2),
            TX_RELAY_REPLY_TOO_MANY_FDS,
            StatusCode::BadValue,
        ),
        (
            Wire::A13(2),
            TX_RELAY_REPLY_WITHOUT_SESSION,
            StatusCode::BadType,
        ),
    ];
    let mut wrong = Vec::new();
    for (wire, code, want) in cases {
        let p = RelayPair::new(wire);
        let rp = rpc_of(&p.root);
        let d = rp.build_request(IRELAY_DESC).unwrap();
        let got = (rp.transact(code, &d, 0).err(), p.root.ping_binder());
        if got != (Some(want), Ok(())) {
            wrong.push((wire, code, got));
        }
        p.finish();
    }
    assert!(wrong.is_empty(), "(wire, code, (status, ping)): {wrong:?}");
}

// ---- a oneway-only client past the server's DEC_STRONG hold ----

/// Socket buffers this small fill within a few hundred `DEC_STRONG` frames on any platform.
const SMALL_SOCKET_BUFFER: usize = 4096;

/// Fills both directions of a macOS loopback TCP connection past the hold (20 000 did not).
const PAST_THE_HOLD: usize = 40_000;

/// A run that drains takes about a second; one that does not never ends.
const FLOOD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

fn small_buffers<F: std::os::fd::AsFd>(s: F) -> F {
    rustix::net::sockopt::set_socket_send_buffer_size(&s, SMALL_SOCKET_BUFFER).expect("SO_SNDBUF");
    rustix::net::sockopt::set_socket_recv_buffer_size(&s, SMALL_SOCKET_BUFFER).expect("SO_RCVBUF");
    s
}

/// The server's end, then the client's.
type Ends = (Box<dyn RpcTransport>, Box<dyn RpcTransport>);

fn small_unix_ends() -> Ends {
    let (a, b) = std::os::unix::net::UnixStream::pair().expect("socketpair");
    (
        Box::new(UnixTransport::from_stream(small_buffers(a)).expect("server end")),
        Box::new(UnixTransport::from_stream(small_buffers(b)).expect("client end")),
    )
}

fn new_child() -> SIBinder {
    Interface::as_binder(&Binder::new(BnChild(Box::new(ChildSvc {
        name: String::new(),
    }))))
}

/// Oneways with new binders past the server's hold drain ("Draining sends"); a twoway frees all.
fn oneway_flood_past_the_hold(name: &str, ends: Ends, wire: Wire, fd: FdMode) {
    let (a, b) = ends;
    let (server, client) = match wire {
        Wire::R34 => (
            RpcSession::new(a, AddressSpace::Acceptor).expect("server"),
            RpcSession::new(b, AddressSpace::Initiator).expect("client"),
        ),
        Wire::A13(v) => {
            let unix = fd == FdMode::Unix;
            let accept = thread::spawn(move || {
                RpcSession::accept_android13plus_fd(a, v, unix).expect("accept")
            });
            let client = RpcSession::connect_android13plus_fd(b, v, fd).expect("connect");
            (accept.join().expect("accept thread"), client)
        }
    };
    server.set_root(make_root()).expect("set_root");
    let server_for_thread = server.clone();
    let serve = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });
    let root = client.get_root().expect("get_root");

    let n = RpcSession::__held_dec_strong_limit() + PAST_THE_HOLD;
    let (tx, rx) = std::sync::mpsc::channel();
    let flood = {
        let root = root.clone();
        thread::spawn(move || {
            let rp = rpc_of(&root);
            let sent = (0..n).try_for_each(|_| {
                let mut d = rp.build_request(ISMOKE_DESC)?;
                d.write(&new_child())?;
                rp.transact(TX_PASS_BINDER, &d, rsbinder::FLAG_ONEWAY)
                    .map(drop)
            });
            let _ = tx.send(sent);
        })
    };
    let outcome = rx.recv_timeout(FLOOD_DEADLINE);
    if outcome.is_err() {
        // Wakes both blocked writers, so the threads can be joined.
        client.close_session();
        server.close_session();
    }
    flood.join().expect("flood thread");
    assert!(
        matches!(outcome, Ok(Ok(()))),
        "{name}: {n} oneways did not go out within {FLOOD_DEADLINE:?}: {outcome:?}"
    );

    assert_eq!(SmokeProxy(root.clone()).echo("flush").unwrap(), "flush");
    assert!(
        poll_until(|| client.local_node_count() == 0),
        "{name}: children the server dropped and the client still holds: {}",
        client.local_node_count()
    );
    drop(root);
    client.close_session();
    serve.join().expect("serve thread");
}

#[test]
fn a_oneway_only_client_drains_past_the_hold_r34_unix() {
    oneway_flood_past_the_hold("r34 unix", small_unix_ends(), Wire::R34, FdMode::None);
}

#[test]
fn a_oneway_only_client_drains_past_the_hold_android13plus_unix() {
    // `Unix` fd mode: the send goes through the `SCM_RIGHTS`-capable path.
    oneway_flood_past_the_hold("a13 unix", small_unix_ends(), Wire::A13(2), FdMode::Unix);
}

#[cfg(feature = "rpc-tcp-debug")]
#[test]
fn a_oneway_only_client_drains_past_the_hold_android13plus_tcp_debug() {
    use rsbinder::rpc::transport::TcpDebugTransport;
    use rustix::net::{AddressFamily, SocketType};
    // Sized before the handshake, which fixes the window scale; the accepted socket inherits.
    let listener = small_buffers(TcpDebugTransport::bind_loopback().expect("bind loopback"));
    let sock = small_buffers(
        rustix::net::socket(AddressFamily::INET, SocketType::STREAM, None).expect("socket"),
    );
    rustix::net::connect(&sock, &listener.local_addr().expect("address")).expect("connect");
    let client = std::net::TcpStream::from(sock);
    let (server, _) = listener.accept().expect("accept");
    let ends: Ends = (
        Box::new(TcpDebugTransport::from_stream(small_buffers(server)).expect("server end")),
        Box::new(TcpDebugTransport::from_stream(small_buffers(client)).expect("client end")),
    );
    oneway_flood_past_the_hold("a13 tcp_debug", ends, Wire::A13(2), FdMode::None);
}

#[cfg(feature = "rpc-tls")]
mod tls_flood {
    use super::*;
    use rsbinder::rpc::rustls::pki_types::pem::PemObject;
    use rsbinder::rpc::rustls::pki_types::{CertificateDer, PrivateKeyDer};
    use rsbinder::rpc::rustls::{ClientConfig, RootCertStore, ServerConfig};
    use rsbinder::rpc::transport::TlsTransport;
    use std::sync::Arc;

    const CA: &str = include_str!("tls_fixtures/ca.crt");
    const SRV_CRT: &str = include_str!("tls_fixtures/srv.crt");
    const SRV_KEY: &str = include_str!("tls_fixtures/srv.key");

    fn certs(pem: &str) -> Vec<CertificateDer<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .collect::<std::result::Result<_, _>>()
            .expect("parse certs")
    }

    /// TLS over a small-buffered unix socketpair, both handshakes done.
    fn small_tls_ends() -> Ends {
        let (s_srv, s_cli) = std::os::unix::net::UnixStream::pair().expect("socketpair");
        let (s_srv, s_cli) = (small_buffers(s_srv), small_buffers(s_cli));
        let srv_cfg = Arc::new(
            ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(
                    certs(SRV_CRT),
                    PrivateKeyDer::from_pem_slice(SRV_KEY.as_bytes()).expect("key"),
                )
                .expect("server config"),
        );
        let server = thread::spawn(move || {
            TlsTransport::accept_stream(Box::new(s_srv), srv_cfg).expect("server handshake")
        });
        let mut roots = RootCertStore::empty();
        for c in certs(CA) {
            roots.add(c).expect("add ca");
        }
        let cli_cfg = Arc::new(
            ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        );
        let client = TlsTransport::connect_stream(Box::new(s_cli), "localhost", cli_cfg)
            .expect("client handshake");
        (
            Box::new(server.join().expect("server thread")),
            Box::new(client),
        )
    }

    #[test]
    fn a_oneway_only_client_drains_past_the_hold_android13plus_tls() {
        oneway_flood_past_the_hold("a13 tls", small_tls_ends(), Wire::A13(2), FdMode::None);
    }

    #[test]
    fn a_oneway_only_client_drains_past_the_hold_r34_tls() {
        oneway_flood_past_the_hold("r34 tls", small_tls_ends(), Wire::R34, FdMode::None);
    }
}

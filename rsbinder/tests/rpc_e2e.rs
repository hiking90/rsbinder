// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-2 end-to-end: a hand-written AIDL-style interface driven
//! over the RPC stack with the **server stub reused unmodified** (the
//! generated free `on_transact` shape, dispatched via
//! `IBinder::rpc_transact` — never `Inner::transact`/`check_interface`,
//! AC-2.7/2.12) and a **hand-written `RpcProxy` client** (D1; the
//! single-stub generator change is subplan 2-6).
//!
//! Separate test binary (not a `src/` unit test) so it never shares a
//! process with the kernel-binder unit tests (master §6). P6: every
//! test builds its own session pair → parallel-safe, no `--test-threads=1`.
//!
//! Covers AC-2.4 (scalar/string/binder-arg e2e over `mem` *and*
//! `unix`), AC-2.5 (DEC_STRONG releases the server node — no leak),
//! AC-2.6 (binder-in-parcel: reply binder → `RpcProxy` → re-call;
//! object-returning-home identity), AC-2.11 (FD reject).

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
const TX_CHILD_NAME: TransactionCode = FIRST_CALL_TRANSACTION;

trait ISmoke: Interface {
    fn echo(&self, s: &str) -> Result<String>;
    fn add(&self, a: i32, b: i32) -> Result<i32>;
    fn get_child(&self) -> Result<SIBinder>;
    /// Returns the descriptor of the binder passed in (exercises a
    /// binder *argument* + the object-returning-home path).
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
        // The address came back from the client; it must resolve to
        // *our* original local child object (object returning home).
        Ok(b.descriptor().to_string())
    }
}

// ---- generated-style free on_transact (the reuse target) ------------
//
// Same shape the AIDL generator emits (generator.rs:367): transport-
// neutral, reads from `reader`, writes `Status` then return value. The
// RPC server reaches this via `IBinder::rpc_transact` -> the generated
// `Remotable::on_transact` — never `check_interface`.

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
        .expect("client binder must be an RpcProxy (P5)")
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

/// Run the full scenario over a connected transport pair. Returns the
/// server session so the caller can assert AC-2.5 node accounting.
fn run_scenario(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root());
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let root = SmokeProxy(client.get_root().expect("get_root"));

        // AC-2.4: scalar + string round-trip, exact values.
        assert_eq!(root.echo("hello rpc").unwrap(), "hello rpc");
        assert_eq!(root.echo("").unwrap(), "");
        assert_eq!(root.add(2, 3).unwrap(), 5);
        assert_eq!(root.add(-7, 7).unwrap(), 0);

        // AC-2.6: reply contains a binder → client builds an RpcProxy
        // → re-calls it.
        let child_sib = root.get_child().unwrap();
        assert!(
            (*child_sib).as_any().downcast_ref::<RpcProxy>().is_some(),
            "AC-2.6: a binder in an RPC reply must become an RpcProxy"
        );
        let child = ChildProxy(child_sib);
        assert_eq!(child.name().unwrap(), "child-1");

        // AC-2.6: binder *argument* — the proxy travels back to the
        // server which recognises it as its own local object.
        assert_eq!(root.pass_binder(&child.0).unwrap(), ICHILD_DESC);

        // AC-2.5: dropping the child proxy sends DEC_STRONG; the next
        // ordered round-trip guarantees the server has processed it,
        // so the child node is released (no leak).
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

/// Subplan 2-6 (D1): an RPC binder obtained from the stack is
/// reachable through the **generalized** `dyn IBinder::as_remote()`
/// as a `&dyn RemoteProxy`, and a full AIDL call driven via the
/// trait's `prepare_transact`/`submit_transact` (the exact shape the
/// generator will emit after 2-6.B) works over RPC — the same trait
/// `ProxyHandle` implements for the kernel path. Proves the single
/// abstraction without any generator change.
#[test]
fn rpc_call_via_generalized_remote_proxy_trait() {
    use rsbinder::RemoteProxy;

    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(make_root());
    let h = thread::spawn(move || {
        let _ = server.serve_blocking();
    });

    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("RpcSession::new");
    let root = client.get_root().expect("get_root");

    // Generalized dispatch: not `as_proxy()` (kernel-only), but
    // `as_remote()` → `&dyn RemoteProxy`. An RPC binder resolves here.
    let remote = (*root)
        .as_remote()
        .expect("AC-6: an RpcProxy must be reachable as &dyn RemoteProxy");

    // The trait's `prepare_transact` is callable on the RPC proxy
    // (it allocates an RPC-mode parcel). Descriptor *stamping* of an
    // RpcProxy from `get_root` is 2-6.B's typed-stub constructor
    // change, so for this pre-2-6.B check the real call is issued with
    // an explicitly-built request parcel and dispatched **through the
    // generalized `&dyn RemoteProxy::submit_transact`** (exactly what
    // the generator will emit after 2-6.B).
    let _ = remote
        .prepare_transact(true)
        .expect("prepare_transact callable");
    let rp = rpc_of(&root);
    let mut d = rp.build_request(ISMOKE_DESC).unwrap();
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

/// AC-2.11 / T2.11: an FD written into an RPC-mode parcel is a hard
/// `BadType` reject (android-12 r34 fidelity), never a silent
/// corruption or partial write.
#[test]
fn rpc_mode_parcel_rejects_file_descriptor() {
    use rsbinder::ParcelFileDescriptor;
    use std::fs::File;

    let mut p = Parcel::new();
    p.set_for_rpc(true);
    let pfd = ParcelFileDescriptor::new(File::open("/dev/null").expect("/dev/null"));
    let err = p
        .write(&pfd)
        .expect_err("FD in RPC parcel must be rejected");
    assert_eq!(err, StatusCode::BadType, "android-12 r34 BAD_TYPE fidelity");

    // Kernel-mode parcel still accepts an FD (no regression).
    let mut k = Parcel::new();
    assert!(!k.is_for_rpc());
    let pfd2 = ParcelFileDescriptor::new(File::open("/dev/null").expect("/dev/null"));
    k.write(&pfd2).expect("kernel-mode FD write still works");
}

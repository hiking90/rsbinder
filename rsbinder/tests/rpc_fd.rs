// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Subplan 2-7: opt-in FD-over-RPC (`FileDescriptorTransportMode`).
//!
//! * AC-7.1 — default (no opt-in) is the 2-2 `BadType` reject,
//!   unchanged.
//! * AC-7.2 — both peers opt in over UDS ⇒ fd travels via
//!   `SCM_RIGHTS`, valid + `O_CLOEXEC` at the receiver; works in
//!   *both* directions (arg and reply).
//! * AC-7.3 — one-sided opt-in falls back to `None` (reject, not an
//!   error).
//! * AC-7.4 — a non-UDS transport (`mem`) never passes fds (rejected
//!   by type at send; zero fds reach the peer).
//!
//! Separate test binary; `#![cfg(feature = "rpc")]`.

#![cfg(feature = "rpc")]

use std::io::{Read, Seek, Write};
use std::os::fd::AsFd;
use std::thread;

use rsbinder::rpc::transport::MemTransport;
use rsbinder::rpc::{
    AddressSpace, FileDescriptorTransportMode as FdMode, RpcProxy, RpcServer, RpcSession,
};
use rsbinder::{
    Binder, Interface, Parcel, ParcelFileDescriptor, Remotable, Result, SIBinder, Status,
    StatusCode, TransactionCode, FIRST_CALL_TRANSACTION,
};

const DESC: &str = "rsbinder.test.IFdSvc";
const TX_LEN_OF: TransactionCode = FIRST_CALL_TRANSACTION; // arg fd → its byte length
const TX_GIVE_FD: TransactionCode = FIRST_CALL_TRANSACTION + 1; // reply carries an fd

trait IFdSvc: Interface {
    fn len_of(&self, fd: &ParcelFileDescriptor) -> Result<i32>;
    fn give_fd(&self) -> Result<ParcelFileDescriptor>;
}

struct FdSvc;
impl Interface for FdSvc {}
impl IFdSvc for FdSvc {
    fn len_of(&self, fd: &ParcelFileDescriptor) -> Result<i32> {
        // The received fd must be a live, readable description.
        let mut f = std::fs::File::from(fd.as_ref().try_clone().map_err(|_| StatusCode::BadFd)?);
        f.rewind().ok();
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).map_err(|_| StatusCode::BadFd)?;
        Ok(buf.len() as i32)
    }
    fn give_fd(&self) -> Result<ParcelFileDescriptor> {
        let mut tf = tempfile();
        tf.write_all(b"from-server").unwrap();
        tf.rewind().unwrap();
        Ok(ParcelFileDescriptor::new(tf))
    }
}

fn fd_on_transact(
    s: &dyn IFdSvc,
    code: TransactionCode,
    reader: &mut Parcel,
    reply: &mut Parcel,
) -> Result<()> {
    match code {
        TX_LEN_OF => {
            let pfd: ParcelFileDescriptor = reader.read()?;
            match s.len_of(&pfd) {
                Ok(v) => {
                    reply.write(&Status::from(StatusCode::Ok))?;
                    reply.write(&v)
                }
                Err(e) => reply.write(&Status::from(e)),
            }
        }
        TX_GIVE_FD => match s.give_fd() {
            Ok(pfd) => {
                reply.write(&Status::from(StatusCode::Ok))?;
                reply.write(&pfd)
            }
            Err(e) => reply.write(&Status::from(e)),
        },
        _ => Err(StatusCode::UnknownTransaction),
    }
}

struct BnFd(Box<dyn IFdSvc + Send + Sync>);
impl Remotable for BnFd {
    fn descriptor() -> &'static str {
        DESC
    }
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        fd_on_transact(&*self.0, code, reader, reply)
    }
    fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> Result<()> {
        Ok(())
    }
}

fn tempfile() -> std::fs::File {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_fd_{}_{}.tmp",
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
        .open(&p)
        .expect("tempfile");
    let _ = std::fs::remove_file(&p); // unlinked; fd keeps it alive
    f
}

fn rp_of(b: &SIBinder) -> &RpcProxy {
    (**b).as_any().downcast_ref::<RpcProxy>().expect("RpcProxy")
}

fn call_len_of(root: &SIBinder, pfd: &ParcelFileDescriptor) -> Result<i32> {
    let rp = rp_of(root);
    let mut d = rp.build_request(DESC)?;
    d.write(pfd)?;
    let mut r = rp
        .transact(TX_LEN_OF, &d, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    let st: Status = r.read()?;
    if !st.is_ok() {
        return Err(StatusCode::from(st));
    }
    r.read::<i32>()
}

fn tmp_sock(tag: &str) -> std::path::PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!(
        "rsb_rpcfd_{}_{}_{}.sock",
        tag,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    p
}
fn wait_sock(p: &std::path::Path) {
    for _ in 0..400 {
        if p.exists() {
            return;
        }
        thread::sleep(std::time::Duration::from_millis(5));
    }
    panic!("socket never appeared");
}

/// AC-7.2: both peers opt in over UDS ⇒ fd passes both ways, valid +
/// O_CLOEXEC at the receiver.
#[test]
fn fd_roundtrip_when_both_opt_in_over_uds() {
    let path = tmp_sock("ok");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    server.set_supported_fd_modes(&[FdMode::Unix]);
    server.set_root(Interface::as_binder(&Binder::new(BnFd(Box::new(FdSvc)))));
    let bg = server.run_background();
    wait_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    assert_eq!(
        client.negotiate_fd_transport(FdMode::Unix).unwrap(),
        FdMode::Unix,
        "both opted in → Unix"
    );
    let root = client.get_root().expect("get_root");

    // arg direction: client passes an fd; server reads its bytes.
    let mut tf = tempfile();
    tf.write_all(b"hello-fd-payload").unwrap();
    tf.rewind().unwrap();
    let pfd = ParcelFileDescriptor::new(tf);
    assert_eq!(call_len_of(&root, &pfd).unwrap(), 16);

    // reply direction: server hands back an fd; client reads it +
    // checks it is O_CLOEXEC.
    let rp = rp_of(&root);
    let d = rp.build_request(DESC).unwrap();
    let mut r = rp.transact(TX_GIVE_FD, &d, 0).unwrap().unwrap();
    let st: Status = r.read().unwrap();
    assert!(st.is_ok());
    let got: ParcelFileDescriptor = r.read().unwrap();
    {
        let mut f = std::fs::File::from(got.as_ref().try_clone().unwrap());
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        assert_eq!(s, "from-server");
    }
    // O_CLOEXEC must be set on the received fd (plan 2-7.d).
    let flags = rustix::io::fcntl_getfd(got.as_ref().as_fd()).unwrap();
    assert!(
        flags.contains(rustix::io::FdFlags::CLOEXEC),
        "received fd must be O_CLOEXEC"
    );

    drop(root);
    drop(client);
    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

/// AC-7.1 / AC-7.3: no opt-in (or one-sided) ⇒ fd write is the 2-2
/// `BadType` reject, never a silent corruption or an error-less hang.
#[test]
fn fd_rejected_without_mutual_opt_in() {
    // (a) server does NOT support Unix; client requests it.
    let path = tmp_sock("noopt");
    let server = RpcServer::setup_unix_server(&path).expect("bind");
    // intentionally NOT set_supported_fd_modes
    server.set_root(Interface::as_binder(&Binder::new(BnFd(Box::new(FdSvc)))));
    let bg = server.run_background();
    wait_sock(&path);

    let client = RpcSession::setup_unix_client(&path).expect("connect");
    assert_eq!(
        client.negotiate_fd_transport(FdMode::Unix).unwrap(),
        FdMode::None,
        "server didn't opt in → fallback to None (not an error)"
    );
    let root = client.get_root().unwrap();
    let mut tf = tempfile();
    tf.write_all(b"x").unwrap();
    let pfd = ParcelFileDescriptor::new(tf);
    assert_eq!(
        call_len_of(&root, &pfd).unwrap_err(),
        StatusCode::BadType,
        "AC-7.1/7.3: FD in None mode is the 2-2 reject"
    );

    drop(root);
    drop(client);
    server.shutdown();
    let _ = bg.join();
    let _ = std::fs::remove_file(&path);
}

/// AC-7.4: a non-UDS transport (`mem`) cannot pass fds — rejected by
/// type at the transport, zero fds reach the peer.
#[test]
fn fd_rejected_on_non_uds_transport() {
    let (a, b) = MemTransport::pair();
    let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor);
    server.set_supported_fd_modes(&[FdMode::Unix]);
    server.set_root(Interface::as_binder(&Binder::new(BnFd(Box::new(FdSvc)))));
    let srv = server.clone();
    let h = thread::spawn(move || {
        let _ = srv.serve_blocking();
    });

    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator);
    // Negotiation itself succeeds logically (both "support" Unix), but
    // the mem transport's fd methods reject by type, so no fd is ever
    // transferred and the call fails cleanly.
    let _ = client.negotiate_fd_transport(FdMode::Unix);
    let root = client.get_root().unwrap();
    let mut tf = tempfile();
    tf.write_all(b"y").unwrap();
    let pfd = ParcelFileDescriptor::new(tf);
    let err = call_len_of(&root, &pfd).expect_err("mem must not pass fds");
    assert!(
        matches!(
            err,
            StatusCode::RpcError | StatusCode::BadType | StatusCode::DeadObject
        ),
        "non-UDS fd attempt must fail cleanly, got {err:?}"
    );

    drop(root);
    drop(client);
    let _ = h.join();
}

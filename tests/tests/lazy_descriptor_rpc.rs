// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 16 AC-16.9: an RPC proxy asks for its interface descriptor when something first needs
//! it, as a kernel proxy does; receiving it or casting it to an interface makes no call.
//!
//! An RPC server answers `INTERFACE_TRANSACTION` before any observer sees it, so each side's
//! transport counts the frames it sends instead: "no call" is "no frame sent" while the binders
//! involved are held, so no reference count goes out either.
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`; every session is in-process.

#![cfg(feature = "rpc")]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use rsbinder::rpc::transport::MemTransport;
use rsbinder::rpc::{AddressSpace, PeerIdentity, RpcResult, RpcServer, RpcSession, RpcTransport};
use rsbinder::{BinderResult, Interface, SIBinder, StatusCode, Strong, Tokio};

include!(concat!(env!("OUT_DIR"), "/lazy_descriptor.rs"));

use lazydesc::ILazyNode::{BnLazyNode, ILazyNode};
use lazydesc::ILazyOther::ILazyOther;

const NODE: &str = "lazydesc.ILazyNode";

/// A memory transport that counts the frames it sends.
struct Counted(MemTransport, Arc<AtomicUsize>);

impl RpcTransport for Counted {
    fn send_frame(&self, buf: &[u8]) -> RpcResult<()> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.send_frame(buf)
    }
    fn recv_frame(&self) -> RpcResult<Vec<u8>> {
        self.0.recv_frame()
    }
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        self.1.fetch_add(1, Ordering::SeqCst);
        self.0.send_raw(buf)
    }
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        self.0.recv_raw(buf)
    }
    fn peer_identity(&self) -> PeerIdentity {
        self.0.peer_identity()
    }
    fn describe(&self) -> &str {
        "counted-mem"
    }
    fn shutdown(&self) -> RpcResult<()> {
        self.0.shutdown()
    }
    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> RpcResult<()> {
        self.0.set_read_timeout(timeout)
    }
}

/// What the server's `takeBinder` saw: frames it sent while converting, and what it had cached.
#[derive(Debug, PartialEq)]
struct Seen {
    frames: usize,
    cached: Option<String>,
}

/// Every conversion plan 16 AC-16.2 lists, none of which may make a call.
fn convert_every_way(peer: &Strong<dyn ILazyNode>) {
    let clone = peer.clone();
    let upgraded = Strong::downgrade(peer).upgrade().expect("Weak::upgrade");
    let sync: Strong<dyn ILazyNode> = clone
        .try_into_async::<Tokio>()
        .expect("try_into_async")
        .into_sync();
    let _: Strong<dyn ILazyNode> = upgraded
        .as_binder()
        .into_interface()
        .expect("into_interface");
    let _ = Strong::<dyn ILazyNode>::try_from(sync.as_binder()).expect("Strong::try_from");
    let _ = format!("{:?}", peer.as_binder());
}

struct Node {
    sent: Arc<AtomicUsize>,
    seen: Arc<Mutex<Option<Seen>>>,
}

impl Interface for Node {}

impl ILazyNode for Node {
    fn r#interfaceQueries(&self) -> BinderResult<i32> {
        Ok(0)
    }
    fn r#takeBinder(&self, peer: &SIBinder) -> BinderResult<()> {
        let before = self.sent.load(Ordering::SeqCst);
        let typed: Strong<dyn ILazyNode> = peer.clone().into_interface()?;
        convert_every_way(&typed);
        let seen = Seen {
            frames: self.sent.load(Ordering::SeqCst) - before,
            cached: peer.cached_descriptor().map(str::to_owned),
        };
        *self.seen.lock().unwrap() = Some(seen);
        Ok(())
    }
    fn r#takeTyped(&self, _: &Strong<dyn ILazyNode>) -> BinderResult<()> {
        Ok(())
    }
    fn r#takeNullable(&self, _: Option<&Strong<dyn ILazyNode>>) -> BinderResult<()> {
        Ok(())
    }
    fn r#askDescriptor(&self, _: &SIBinder) -> BinderResult<String> {
        Err(StatusCode::UnknownTransaction.into())
    }
}

/// A served `ILazyNode` root and a client session, each side counting what it sends.
struct Pair {
    client: RpcSession,
    sent: Arc<AtomicUsize>,
    seen: Arc<Mutex<Option<Seen>>>,
    serving: Option<thread::JoinHandle<()>>,
}

impl Pair {
    fn new() -> Self {
        let (a, b) = MemTransport::pair();
        let (sent, server_sent) = (Arc::default(), Arc::new(AtomicUsize::new(0)));
        let seen = Arc::default();
        let server = RpcSession::new(
            Box::new(Counted(a, server_sent.clone())),
            AddressSpace::Acceptor,
        )
        .expect("server session");
        let node = Node {
            sent: server_sent,
            seen: Arc::clone(&seen),
        };
        server
            .set_root(BnLazyNode::new_binder(node).as_binder())
            .expect("set_root");
        let serving = thread::spawn(move || {
            let _ = server.serve_blocking();
        });
        let client = RpcSession::new(
            Box::new(Counted(b, Arc::clone(&sent))),
            AddressSpace::Initiator,
        )
        .expect("client session");
        Pair {
            client,
            sent,
            seen,
            serving: Some(serving),
        }
    }

    fn sent(&self) -> usize {
        self.sent.load(Ordering::SeqCst)
    }
}

impl Drop for Pair {
    fn drop(&mut self) {
        self.client.close_session();
        if let Some(serving) = self.serving.take() {
            let _ = serving.join();
        }
    }
}

/// A binder of this process, for the server to receive.
fn local_node() -> SIBinder {
    let node = Node {
        sent: Arc::default(),
        seen: Arc::default(),
    };
    BnLazyNode::new_binder(node).as_binder()
}

#[test]
fn a_received_rpc_proxy_is_cast_every_way_without_a_call() {
    let pair = Pair::new();
    let root = pair.client.get_root().expect("get_root");

    let before = pair.sent();
    let typed: Strong<dyn ILazyNode> = root.clone().into_interface().expect("into_interface");
    convert_every_way(&typed);
    assert_eq!(pair.sent() - before, 0, "a cast made a call");
    assert_eq!(root.cached_descriptor(), None);

    // The server receives a binder of this process and casts it the same ways.
    typed.r#takeBinder(&local_node()).expect("takeBinder");
    let seen = pair.seen.lock().unwrap().take();
    assert_eq!(
        seen,
        Some(Seen {
            frames: 0,
            cached: None
        })
    );
}

#[test]
fn a_cast_to_another_interface_decides_nothing() {
    let pair = Pair::new();
    let root = pair.client.get_root().expect("get_root");

    let other: Strong<dyn ILazyOther> = root.clone().into_interface().expect("unchecked cast");
    let node: Strong<dyn ILazyNode> = root.clone().into_interface().expect("a second cast");
    assert_eq!(
        other.r#poke().unwrap_err().transaction_error(),
        StatusCode::BadType
    );
    assert_eq!(
        node.r#interfaceQueries()
            .expect("the right interface answers"),
        0
    );
    assert_eq!(root.descriptor(), NODE);
}

#[test]
fn the_descriptor_is_asked_once_when_first_needed() {
    let pair = Pair::new();
    let root = pair.client.get_root().expect("get_root");

    let before = pair.sent();
    assert_eq!(root.descriptor(), NODE);
    let asked = pair.sent();
    assert!(asked > before, "the first descriptor() sent nothing");
    assert_eq!(root.try_descriptor(), Ok(NODE));
    assert_eq!(root.cached_descriptor(), Some(NODE));
    assert_eq!(pair.sent(), asked, "a cached descriptor was asked again");
}

#[test]
fn a_checked_cast_refuses_another_interface() {
    let pair = Pair::new();
    let root = pair.client.get_root().expect("get_root");
    assert_eq!(
        root.clone()
            .into_interface_checked::<dyn ILazyOther>()
            .err(),
        Some(StatusCode::BadType)
    );
    assert!(root.into_interface_checked::<dyn ILazyNode>().is_ok());
}

#[test]
fn get_interface_refuses_another_interface() {
    let path = std::env::temp_dir().join(format!("rsb_lazydesc_{}.sock", std::process::id()));
    let server = RpcServer::setup_unix_server(&path).expect("server object");
    server
        .add_service("node", local_node())
        .expect("add_service");
    let (a, b) = MemTransport::pair();
    server.serve_connection(Box::new(a));
    let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).expect("client session");

    assert_eq!(
        client.get_interface::<dyn ILazyOther>("node").err(),
        Some(StatusCode::BadType)
    );
    assert!(client.get_interface::<dyn ILazyNode>("node").is_ok());
    client.close_session();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn a_failed_query_is_returned_and_leaves_the_cache_empty() {
    let pair = Pair::new();
    let root = pair.client.get_root().expect("get_root");
    pair.client.close_session();

    assert_eq!(root.try_descriptor(), Err(StatusCode::DeadObject));
    assert_eq!(root.descriptor(), "");
    assert_eq!(root.cached_descriptor(), None);
    let remote = root.as_remote().expect("an RPC root is a proxy");
    assert_eq!(
        remote.prepare_transact(true).err(),
        Some(StatusCode::DeadObject)
    );
}

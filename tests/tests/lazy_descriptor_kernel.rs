// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 16: receiving a kernel binder, or casting it to an interface, makes no call to the
//! process behind it; the interface descriptor is asked for when something first needs it.
//!
//! Each node is this test binary re-executed as `child` with [`ROLE`] set. It serves
//! `ILazyNode` on `FatalIfNotOneway` binder threads, counts the `INTERFACE_TRANSACTION`s it
//! serves with a transaction observer, says `ready <count>` on stdout once registered, and exits
//! when its stdin closes. The count it says includes the service manager's own query at
//! `addService`, if it made one.
//!
//! Requires a binder device and a service manager that accepts `addService` for any name
//! (`rsb_hub --insecure-allow-all`, or the kernel tier's permissive policy):
//!
//! ```text
//! cargo test -p tests --test lazy_descriptor_kernel -- --ignored --test-threads=1
//! ```

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::io::{BufRead, BufReader};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{mpsc, Arc, Once};
use std::time::Duration;

use rsbinder::observe::{TransactionObserver, TxnContext};
use rsbinder::{
    hub, BinderResult, CallRestriction, Interface, ProcessState, SIBinder, StatusCode, Strong,
    Tokio, INTERFACE_TRANSACTION,
};

include!(concat!(env!("OUT_DIR"), "/lazy_descriptor.rs"));

use lazydesc::ILazyNode::{BnLazyNode, ILazyNode};
use lazydesc::ILazyOther::ILazyOther;

const ROLE: &str = "RSB_LAZYDESC_ROLE";
const NAME: &str = "RSB_LAZYDESC_NAME";
const WAIT: Duration = Duration::from_secs(10);
const NODE: &str = "lazydesc.ILazyNode";

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
    });
}

fn name(tag: &str) -> String {
    format!("rsb.test.lazydesc.{tag}.{}", std::process::id())
}

static QUERIES: AtomicI32 = AtomicI32::new(0);

struct CountQueries;

impl TransactionObserver for CountQueries {
    fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn std::any::Any + Send>> {
        if ctx.code == INTERFACE_TRANSACTION {
            QUERIES.fetch_add(1, Ordering::SeqCst);
        }
        None
    }
    fn on_reply(
        &self,
        _: &TxnContext<'_>,
        _: Option<Box<dyn std::any::Any + Send>>,
        _: &rsbinder::Result<()>,
        _: Duration,
    ) {
    }
}

/// Every conversion plan 16 AC-16.2 lists, none of which may call `peer`'s process.
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

struct Node;

impl Interface for Node {}

impl ILazyNode for Node {
    fn r#interfaceQueries(&self) -> BinderResult<i32> {
        Ok(QUERIES.load(Ordering::SeqCst))
    }
    fn r#takeBinder(&self, _: &SIBinder) -> BinderResult<()> {
        Ok(())
    }
    fn r#takeTyped(&self, peer: &Strong<dyn ILazyNode>) -> BinderResult<()> {
        convert_every_way(peer);
        Ok(())
    }
    fn r#takeNullable(&self, peer: Option<&Strong<dyn ILazyNode>>) -> BinderResult<()> {
        convert_every_way(peer.ok_or(StatusCode::UnexpectedNull)?);
        Ok(())
    }
    fn r#askDescriptor(&self, peer: &SIBinder) -> BinderResult<String> {
        Ok(peer.try_descriptor()?.to_owned())
    }
}

/// The node role (module doc); it registers first, as its binder threads refuse twoway calls.
#[test]
#[ignore = "a child role of the tests below"]
fn child() {
    let Ok(_role) = std::env::var(ROLE) else {
        return;
    };
    let name = std::env::var(NAME).expect("a service name");
    rsbinder::observe::set_observer(Some(Arc::new(CountQueries)));
    let process = ProcessState::init_default().expect("a binder device");
    hub::add_service(&name, BnLazyNode::new_binder(Node)).expect("addService");
    process.set_call_restriction(CallRestriction::FatalIfNotOneway);
    ProcessState::start_thread_pool();
    println!("lazydesc: ready {}", QUERIES.load(Ordering::SeqCst));
    let mut line = String::new();
    while std::io::stdin()
        .read_line(&mut line)
        .is_ok_and(|read| read != 0)
    {
        line.clear();
    }
}

/// A node process, killed when dropped.
struct Peer {
    child: Child,
    _stdin: ChildStdin,
    name: String,
    /// The count the node said at `ready`.
    queries: i32,
}

impl Peer {
    fn spawn(tag: &str) -> Self {
        let name = name(tag);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, "node")
            .env(NAME, &name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, ready) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Not only at the start: libtest's `test child ... ` has no newline before it.
                if let Some((_, count)) = line.split_once("lazydesc: ready ") {
                    let _ = send.send(count.trim().parse::<i32>().unwrap());
                }
            }
        });
        let stdin = child.stdin.take().unwrap();
        let queries = ready.recv_timeout(WAIT).expect("the node to register");
        Self {
            child,
            _stdin: stdin,
            name,
            queries,
        }
    }

    /// This node as a kernel proxy, read from the service manager's reply.
    fn binder(&self) -> SIBinder {
        hub::check_service(&self.name).expect("a registered node")
    }

    /// `SIGSTOP` until the guard drops, also when the test fails.
    fn stop(&self) -> Stopped {
        let pid = rustix::process::Pid::from_raw(self.child.id() as i32).unwrap();
        rustix::process::kill_process(pid, rustix::process::Signal::STOP).unwrap();
        // Until every thread stopped: a call that follows meets no running thread of it.
        let stopped =
            rustix::process::WaitIdOptions::STOPPED | rustix::process::WaitIdOptions::NOWAIT;
        rustix::process::waitid(rustix::process::WaitId::Pid(pid), stopped).unwrap();
        Stopped(pid)
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Sends `SIGCONT` when dropped; drop it before its [`Peer`].
#[must_use = "the node runs again when the guard is dropped"]
struct Stopped(rustix::process::Pid);

impl Drop for Stopped {
    fn drop(&mut self) {
        let _ = rustix::process::kill_process(self.0, rustix::process::Signal::CONT);
    }
}

/// Runs `f` on another thread and fails the test if it does not return within [`WAIT`].
fn within_wait(what: &str, f: impl FnOnce() + Send + 'static) {
    let (done, finished) = mpsc::channel();
    std::thread::spawn(move || {
        f();
        let _ = done.send(());
    });
    match finished.recv_timeout(WAIT) {
        Ok(()) => {}
        Err(mpsc::RecvTimeoutError::Timeout) => {
            panic!("{what} within {WAIT:?}: it waited on the stopped node")
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => panic!("{what} panicked"),
    }
}

/// AC-16.1, AC-16.2: a stopped node's binder is received and converted every way without waiting.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn a_stopped_nodes_binder_is_received_and_converted_without_waiting() {
    init();
    let stopped_node = Peer::spawn("stopped");
    let receiver = Peer::spawn("receiver");
    let peer = stopped_node.binder();
    let typed: Strong<dyn ILazyNode> = peer.clone().into_interface().unwrap();
    let receiver: Strong<dyn ILazyNode> = hub::check_interface(&receiver.name).unwrap();

    let stopped = stopped_node.stop();
    within_wait("the receiver's calls", {
        let (peer, typed) = (peer.clone(), typed.clone());
        move || {
            receiver.r#takeBinder(&peer).unwrap();
            receiver.r#takeTyped(&typed).unwrap();
            receiver.r#takeNullable(Some(&typed)).unwrap();
        }
    });
    within_wait("this process's conversions", {
        let typed = typed.clone();
        move || convert_every_way(&typed)
    });
    drop(stopped);

    assert_eq!(
        typed.r#interfaceQueries().unwrap(),
        stopped_node.queries,
        "the stopped node served no INTERFACE_TRANSACTION"
    );
}

/// AC-16.3, 16.4, 16.7: only `descriptor()` asks, once; lookup, unchecked cast, call, Debug don't.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn the_descriptor_is_asked_once_when_first_needed() {
    init();
    let node = Peer::spawn("asked");
    let peer = node.binder();
    let typed: Strong<dyn ILazyNode> = peer.clone().into_interface().unwrap();
    assert!(format!("{peer:?}").contains("(not cached)"), "{peer:?}");
    assert_eq!(peer.cached_descriptor(), None);
    assert_eq!(typed.r#interfaceQueries().unwrap(), node.queries);

    assert_eq!(peer.descriptor(), NODE);
    assert_eq!(typed.r#interfaceQueries().unwrap(), node.queries + 1);
    assert_eq!(peer.try_descriptor(), Ok(NODE));
    assert_eq!(node.binder().descriptor(), NODE);
    assert!(format!("{peer:?}").contains(NODE), "{peer:?}");
    assert_eq!(
        typed.r#interfaceQueries().unwrap(),
        node.queries + 1,
        "answered from the cache"
    );
}

/// AC-16.3, AC-16.5: a failed query returns its error and is not cached.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn a_failed_query_is_returned_and_asked_again() {
    init();
    let mut node = Peer::spawn("dead");
    let peer = node.binder();
    node.kill();

    assert_eq!(peer.try_descriptor(), Err(StatusCode::DeadObject));
    assert_eq!(peer.descriptor(), "");
    assert_eq!(peer.cached_descriptor(), None, "a failure is not cached");
    assert_eq!(peer.try_descriptor(), Err(StatusCode::DeadObject));
    let proxy = peer.as_proxy().expect("a kernel proxy");
    assert_eq!(
        proxy.prepare_transact(true).err(),
        Some(StatusCode::DeadObject)
    );
    assert_eq!(
        peer.into_interface_checked::<dyn ILazyNode>().err(),
        Some(StatusCode::DeadObject)
    );
}

/// AC-16.10, D2 (a): a `FatalIfNotOneway` thread's first descriptor ask sends nothing and fails.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn a_restricted_thread_asks_no_descriptor() {
    init();
    let asker = Peer::spawn("asker");
    let asked = Peer::spawn("asked");
    let asker_typed: Strong<dyn ILazyNode> = asker.binder().into_interface().unwrap();
    let asked_typed: Strong<dyn ILazyNode> = asked.binder().into_interface().unwrap();

    let refused = asker_typed.r#askDescriptor(&asked.binder()).unwrap_err();
    assert_eq!(
        refused.transaction_error(),
        StatusCode::Unknown,
        "{refused}"
    );
    assert_eq!(
        asked_typed.r#interfaceQueries().unwrap(),
        asked.queries,
        "nothing was sent to the asked node"
    );
}

/// AC-16.5: a checked cast refuses another interface; an unchecked one fails at the first call.
#[test]
#[ignore = "requires a binder device and a permissive service manager"]
fn another_interface_is_refused_by_a_checked_cast_or_the_first_call() {
    init();
    let node = Peer::spawn("other");
    let peer = node.binder();

    assert_eq!(
        hub::check_interface::<dyn ILazyOther>(&node.name).err(),
        Some(StatusCode::BadType)
    );
    assert_eq!(
        peer.clone()
            .into_interface_checked::<dyn ILazyOther>()
            .err(),
        Some(StatusCode::BadType)
    );
    let unchecked: Strong<dyn ILazyOther> = peer.clone().into_interface().unwrap();
    assert_eq!(
        unchecked.r#poke().unwrap_err().transaction_error(),
        StatusCode::BadType
    );
    let node_typed = hub::check_interface::<dyn ILazyNode>(&node.name).expect("its own interface");
    assert_eq!(node_typed.r#interfaceQueries().unwrap(), node.queries + 1);
}

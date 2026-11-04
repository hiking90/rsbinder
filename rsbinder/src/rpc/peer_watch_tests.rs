// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A session that nothing reads between its calls learns of its peer's close (`session` module doc
//! "A peer's close"). Hermetic: raw peers on Unix socketpairs. TLS, `from_preconnected_fd` and the
//! transports' `peer_close_fd` have their tests in `tests/`.
//!
//! # Mutation gates
//!
//! - `a_session_nothing_reads_runs_its_recipient_when_the_peer_closes`: start no thread.
//! - `a_call_in_flight_keeps_its_reply_when_the_peer_closes`: end the session at the signal, and
//!   the binder in the `REPLY` enters a torn-down session.
//! - `another_connections_close_waits_for_the_call_on_this_one`: record only the slot that closed.
//! - `the_signal_reads_no_frame`: read the slot in the job, and the handler runs.
//! - `a_slot_used_over_and_over_does_not_hold_the_end_back`: look for a free moment with `slot_cv`.
//! - `a_serve_loop_begun_after_the_link_keeps_the_end_to_itself`: drop the `notices` tests from the
//!   two jobs.
//! - `the_watcher_keeps_no_session_alive`: have the thread hold an `Arc` of the session, or keep
//!   its write end outside the pool.
//! - `a_connection_added_later_is_watched`: drop the byte from `PeerWatch::add`.
//! - `a_slot_added_after_the_watcher_ended_raises_no_signal`: write it to a pipe.
//! - `frames_for_the_calls_do_not_wake_the_watcher`: ask for `POLLIN` on the sockets.
//! - `a_failed_serve_spawn_watches_in_its_place`: drop `after_a_failed_serve_spawn`.

use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex, Weak};
use std::time::Duration;

use super::super::address::{AddressSpace, RpcAddress};
use super::super::proxy::RpcProxy;
use super::super::transport::{MemTransport, RpcTransport, UnixTransport};
use super::super::wire::{WireReply, WireTransaction};
use super::super::wire_android13::{read_aosp_message, write_aosp_message, Android13PlusCodec};
use super::super::{RpcResult, RpcSession};
use super::*;
use crate::binder::{DeathRecipient, WIBinder};
use crate::{Binder, Interface, Parcel, Remotable, SIBinder, StatusCode, TransactionCode};

const WAIT: Duration = Duration::from_secs(5);
/// How long a thing that must not happen is given to happen.
const QUIET: Duration = Duration::from_millis(200);

fn codec() -> Android13PlusCodec {
    Android13PlusCodec::with_version(PROTOCOL_V2).expect("v2")
}

fn client() -> (RpcSession, UnixStream) {
    let (ours, peer) = UnixStream::pair().expect("socketpair");
    peer.set_read_timeout(Some(WAIT)).expect("peer timeout");
    let session = RpcSession::with_profile(
        Box::new(UnixTransport::from_stream(ours).expect("transport")),
        AddressSpace::Initiator,
        WireProfile::Android13Plus(codec()),
    )
    .expect("session");
    (session, peer)
}

/// One more connection on `session`, and its peer's end.
fn another_connection(session: &RpcSession) -> UnixStream {
    let (ours, peer) = UnixStream::pair().expect("socketpair");
    peer.set_read_timeout(Some(WAIT)).expect("peer timeout");
    let ours = UnixTransport::from_stream(ours).expect("transport");
    session
        .inner
        .add_outgoing_slot(Box::new(ours))
        .expect("the slot");
    peer
}

/// Runs on the first obituary; tells which thread.
struct Told(Mutex<mpsc::Sender<std::thread::ThreadId>>);

impl DeathRecipient for Told {
    fn binder_died(&self, _: &WIBinder) {
        let told = self.0.lock().expect("told");
        let _ = told.send(std::thread::current().id());
    }
}

/// A proxy the session's obituaries reach.
fn a_proxy(session: &RpcSession) -> SIBinder {
    let mut counter = 7;
    let addr = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
    let inner = session.inner.clone();
    let (binder, _) = session
        .inner
        .shared
        .state
        .lock()
        .expect("state")
        .remote_proxy(addr, || {
            SIBinder::new(Arc::new(RpcProxy::new(addr, inner))).expect("proxy")
        })
        .expect("a proxy");
    binder
}

/// `a_proxy`, linked: the recipient lives as long as the first two are kept.
fn linked(session: &RpcSession) -> (SIBinder, Arc<Told>, mpsc::Receiver<std::thread::ThreadId>) {
    let (tx, rx) = mpsc::channel();
    let proxy = a_proxy(session);
    let told = Arc::new(Told(Mutex::new(tx)));
    let weak: Weak<Told> = Arc::downgrade(&told);
    proxy.link_to_death(weak).expect("the link");
    (proxy, told, rx)
}

fn watcher_polls(session: &RpcSession) -> Arc<AtomicUsize> {
    let st = session.inner.conn_state.lock().expect("conn_state");
    st.watcher.as_ref().expect("a watcher").polls.clone()
}

/// Until `done`, or a panic.
fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn held_slots(session: &RpcSession) -> usize {
    let st = session.inner.conn_state.lock().expect("conn_state");
    st.holder_wait.len()
}

/// A local node the peer holds, whose `Drop` waits for the gate, as a user's `Drop` might.
struct SlowDrop(Mutex<mpsc::Receiver<()>>, mpsc::Sender<()>);

impl Drop for SlowDrop {
    fn drop(&mut self) {
        let _ = self.1.send(());
        let _ = self.0.lock().expect("gate").recv_timeout(WAIT);
    }
}

impl Interface for SlowDrop {}

impl Remotable for SlowDrop {
    fn descriptor() -> &'static str {
        "rsbinder.test.ISlowDrop"
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        Ok(())
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

/// A `REPLY` carrying a binder at `addr`, wire v2.
fn reply_with_binder(addr: RpcAddress) -> Vec<u8> {
    let mut data = 1i32.to_le_bytes().to_vec();
    data.extend_from_slice(&Android13PlusCodec::encode_addr(&addr));
    data.extend_from_slice(&0i32.to_le_bytes());
    codec()
        .encode_reply(&WireReply {
            status: 0,
            data,
            object_positions: vec![0],
        })
        .expect("a reply")
}

#[test]
fn a_session_nothing_reads_runs_its_recipient_when_the_peer_closes() {
    let (session, peer) = client();
    let (_proxy, _told, died) = linked(&session);
    assert!(died.recv_timeout(QUIET).is_err(), "the peer is still there");
    drop(peer);
    let on = died.recv_timeout(WAIT).expect("the recipient");
    assert_ne!(on, std::thread::current().id());
    assert!(session.is_ended());
}

#[test]
fn an_r34_session_runs_its_recipient_when_the_peer_closes() {
    let (ours, peer) = UnixTransport::pair().expect("socketpair");
    let session = RpcSession::new(Box::new(ours), AddressSpace::Initiator).expect("session");
    let (_proxy, _told, died) = linked(&session);
    drop(peer);
    died.recv_timeout(WAIT).expect("the recipient");
}

#[cfg(feature = "rpc-tcp-debug")]
#[test]
fn a_tcp_session_runs_its_recipient_when_the_peer_closes() {
    use crate::rpc::transport::TcpDebugTransport;
    let (ours, peer) = TcpDebugTransport::pair_loopback().expect("a pair");
    let session = RpcSession::with_profile(
        Box::new(ours),
        AddressSpace::Initiator,
        WireProfile::Android13Plus(codec()),
    )
    .expect("session");
    let (_proxy, _told, died) = linked(&session);
    drop(peer);
    died.recv_timeout(WAIT).expect("the recipient");
}

/// A peer that shuts only its writing side is gone as a reader of the connection sees it.
#[test]
fn a_peers_half_close_ends_the_session() {
    let (session, peer) = client();
    let (_proxy, _told, died) = linked(&session);
    peer.shutdown(std::net::Shutdown::Write)
        .expect("half close");
    died.recv_timeout(WAIT).expect("the recipient");
    assert!(session.is_ended());
}

/// The peer wrote a `DEC_STRONG` and a `REPLY` with a binder, and closed: the call gets its reply.
#[test]
fn a_call_in_flight_keeps_its_reply_when_the_peer_closes() {
    let (session, mut peer) = client();
    let (_proxy, _told, died) = linked(&session);
    let (open_gate, gate) = mpsc::channel();
    let (dropping, in_drop) = mpsc::channel();
    let node: SIBinder = Binder::new(SlowDrop(Mutex::new(gate), dropping)).as_binder();
    let node_addr = session
        .inner
        .shared
        .state
        .lock()
        .expect("state")
        .on_binder_leaving(&node)
        .expect("an address");
    drop(node);
    let caller = {
        let inner = session.inner.clone();
        std::thread::spawn(move || {
            let mut counter = 40;
            let target = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
            let mut reply = inner
                .client_transact(target, 1, &Parcel::new(), 0)?
                .ok_or(StatusCode::UnexpectedNull)?;
            reply.read::<SIBinder>().map(drop)
        })
    };
    read_aosp_message(&mut peer).expect("the request");
    let mut counter = 50;
    let returned = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
    let (frame, _) = codec().encode_dec_strong(&node_addr, 1).expect("a frame");
    write_aosp_message(&mut peer, &frame).expect("the release");
    write_aosp_message(&mut peer, &reply_with_binder(returned)).expect("the reply");
    drop(peer);
    in_drop
        .recv_timeout(WAIT)
        .expect("the wait read the release");
    eventually("the job saw the holder", || held_slots(&session) == 1);
    assert!(
        died.recv_timeout(QUIET).is_err(),
        "the holder has not let go"
    );
    assert!(!session.is_ended());
    open_gate.send(()).expect("the gate");
    assert_eq!(caller.join().expect("the caller"), Ok(()));
    died.recv_timeout(WAIT).expect("the recipient");
}

/// One connection closing is the session's end, but not before a call on another has its reply.
#[test]
fn another_connections_close_waits_for_the_call_on_this_one() {
    let (session, mut peer) = client();
    let second = another_connection(&session);
    let (_proxy, _told, died) = linked(&session);
    let caller = {
        let inner = session.inner.clone();
        std::thread::spawn(move || {
            let mut counter = 40;
            let target = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
            let reply = inner.client_transact(target, 1, &Parcel::new(), 0)?;
            reply
                .ok_or(StatusCode::UnexpectedNull)?
                .read::<SIBinder>()
                .map(drop)
        })
    };
    read_aosp_message(&mut peer).expect("the request");
    drop(second);
    eventually("the job saw the holder", || held_slots(&session) == 1);
    assert!(
        died.recv_timeout(QUIET).is_err(),
        "the holder has not let go"
    );
    let mut counter = 50;
    let returned = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
    write_aosp_message(&mut peer, &reply_with_binder(returned)).expect("the reply");
    assert_eq!(caller.join().expect("the caller"), Ok(()));
    died.recv_timeout(WAIT).expect("the recipient");
}

/// A frame the peer left stays unread: no handler runs on the reaper, and the node goes anyway.
#[test]
fn the_signal_reads_no_frame() {
    struct Counts(Arc<AtomicUsize>);
    impl Interface for Counts {}
    impl Remotable for Counts {
        fn descriptor() -> &'static str {
            "rsbinder.test.ICounts"
        }
        fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
            Ok(())
        }
    }
    let (session, mut peer) = client();
    let (_proxy, _told, died) = linked(&session);
    let runs = Arc::new(AtomicUsize::new(0));
    let node: SIBinder = Binder::new(Counts(runs.clone())).as_binder();
    let addr = session
        .inner
        .shared
        .state
        .lock()
        .expect("state")
        .on_binder_leaving(&node)
        .expect("an address");
    let frame = codec()
        .encode_transact(&WireTransaction {
            address: addr,
            code: crate::FIRST_CALL_TRANSACTION,
            flags: FLAG_ONEWAY,
            async_number: 0,
            data: Vec::new(),
            object_positions: Vec::new(),
        })
        .expect("a frame");
    write_aosp_message(&mut peer, &frame).expect("the frame");
    drop(peer);
    died.recv_timeout(WAIT).expect("the recipient");
    assert_eq!(runs.load(Ordering::SeqCst), 0);
    assert_eq!(session.inner.shared.local_node_count(), 0, "`clear_local`");
    drop(node);
}

/// A thread that keeps taking the first slot leaves it no free moment worth looking for.
#[test]
fn a_slot_used_over_and_over_does_not_hold_the_end_back() {
    let (session, mut first) = client();
    let second = another_connection(&session);
    let (_proxy, _told, died) = linked(&session);
    let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let answering = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            let reply = codec().encode_reply(&WireReply::default()).expect("reply");
            while !stop.load(Ordering::SeqCst) {
                if read_aosp_message(&mut first).is_err() {
                    return;
                }
                let _ = write_aosp_message(&mut first, &reply);
            }
        })
    };
    let caller = {
        let inner = session.inner.clone();
        std::thread::spawn(move || {
            let mut counter = 40;
            let target = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
            while inner.client_transact(target, 1, &Parcel::new(), 0).is_ok() {}
        })
    };
    std::thread::sleep(Duration::from_millis(20));
    drop(second);
    let told = died.recv_timeout(WAIT);
    stop.store(true, Ordering::SeqCst);
    session.close_session();
    caller.join().expect("the caller");
    answering.join().expect("the peer");
    told.expect("the recipient, with a thread on the first slot");
}

/// A serve loop begun after the link keeps the session's end to itself, and its value.
#[test]
fn a_serve_loop_begun_after_the_link_keeps_the_end_to_itself() {
    let (session, mut first) = client();
    let mut second = another_connection(&session);
    let (_proxy, _told, died) = linked(&session);
    let serving = session.spawn_serve().expect("the loop");
    // A release this end owes, which goes out on the free slot: the reaper is not held up.
    let mut counter = 40;
    let owed = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
    second
        .shutdown(std::net::Shutdown::Write)
        .expect("half close");
    let polls = watcher_polls(&session);
    eventually("the signal", || polls.load(Ordering::SeqCst) == 1);
    let (frame, _) = codec().encode_dec_strong(&owed, 1).expect("a frame");
    write_aosp_message(&mut first, &frame).expect("a frame for the loop");
    assert!(died.recv_timeout(QUIET).is_err(), "the loop reads on");
    assert!(!session.is_ended());
    session.inner.reap_dec_strong(owed, 1);
    read_aosp_message(&mut second).expect("what the reaper owed");
    drop(first);
    let end = serving.join().expect("the loop");
    assert_eq!(end.reason, EndReason::EndOfStream);
    died.recv_timeout(WAIT).expect("the recipient");
}

/// The last handle's drop ends the thread, and the peer sees the end: nothing keeps it open.
#[test]
fn the_watcher_keeps_no_session_alive() {
    let (session, peer) = client();
    let (proxy, told, died) = linked(&session);
    let weak = session.inner_weak();
    drop(proxy);
    drop(session);
    let deadline = Instant::now() + WAIT;
    while weak.upgrade().is_some() {
        assert!(Instant::now() < deadline, "the session stayed alive");
        std::thread::sleep(Duration::from_millis(2));
    }
    let peer = UnixTransport::from_stream(peer).expect("transport");
    peer.set_read_timeout(Some(WAIT)).expect("deadline");
    let mut bytes = [0u8; 256];
    // A `DEC_STRONG` of the proxy comes first.
    while peer.recv_raw(&mut bytes).expect("the end of stream") > 0 {}
    assert!(
        died.recv_timeout(QUIET).is_err(),
        "a local end tells no one"
    );
    drop(told);
}

#[test]
fn a_close_session_with_a_watcher_waits_for_nothing() {
    let (session, _peer) = client();
    let (_proxy, _told, died) = linked(&session);
    let ending = {
        let session = session.clone();
        std::thread::spawn(move || session.close_session())
    };
    died.recv_timeout(WAIT).expect("the recipient");
    ending.join().expect("the close");
    let st = session.inner.conn_state.lock().expect("conn_state");
    assert!(st.watcher.is_none(), "the session's end takes it");
}

/// A recipient may end what is already ending: the reaper it runs on joins nothing of its own.
#[test]
fn a_recipient_on_the_reaper_can_call_close_session() {
    struct Closes(RpcSession, Mutex<mpsc::Sender<()>>);
    impl DeathRecipient for Closes {
        fn binder_died(&self, _: &WIBinder) {
            self.0.close_session();
            let _ = self.1.lock().expect("told").send(());
        }
    }
    let (session, peer) = client();
    let proxy = a_proxy(&session);
    let (tx, closed) = mpsc::channel();
    let recipient = Arc::new(Closes(session.clone(), Mutex::new(tx)));
    let weak: Weak<Closes> = Arc::downgrade(&recipient);
    proxy.link_to_death(weak).expect("the link");
    drop(peer);
    closed.recv_timeout(WAIT).expect("the recipient");
}

/// A `shutdown` that does nothing leaves the watcher no signal: the session's end stops it anyway.
#[test]
fn a_transport_whose_shutdown_does_nothing_lets_its_socket_go() {
    struct NoShutdown(UnixTransport);
    impl RpcTransport for NoShutdown {
        fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
            self.0.send_raw(buf)
        }
        fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
            self.0.recv_raw(buf)
        }
        fn peer_close_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
            self.0.peer_close_fd()
        }
        fn peer_identity(&self) -> PeerIdentity {
            self.0.peer_identity()
        }
        fn describe(&self) -> &str {
            self.0.describe()
        }
        fn shutdown(&self) -> RpcResult<()> {
            Ok(())
        }
    }
    let (ours, peer) = UnixTransport::pair().expect("socketpair");
    peer.set_read_timeout(Some(WAIT)).expect("deadline");
    let session = RpcSession::with_profile(
        Box::new(NoShutdown(ours)),
        AddressSpace::Initiator,
        WireProfile::Android13Plus(codec()),
    )
    .expect("session");
    let (proxy, _told, died) = linked(&session);
    let polls = watcher_polls(&session);
    session.close_session();
    died.recv_timeout(WAIT).expect("the recipient");
    drop(proxy);
    drop(session);
    let mut bytes = [0u8; 256];
    while peer.recv_raw(&mut bytes).expect("the end of stream") > 0 {}
    assert_eq!(polls.load(Ordering::SeqCst), 1, "woken by its wake's close");
}

/// Only what has no socket to watch is refused; a refusal starts nothing.
#[test]
fn a_connection_with_no_socket_refuses_the_link() {
    let (ours, _peer) = MemTransport::pair();
    let session = RpcSession::with_profile(
        Box::new(ours),
        AddressSpace::Initiator,
        WireProfile::Android13Plus(codec()),
    )
    .expect("session");
    let (tx, _died) = mpsc::channel();
    let told = Arc::new(Told(Mutex::new(tx)));
    let weak: Weak<Told> = Arc::downgrade(&told);
    let refused = a_proxy(&session).link_to_death(weak);
    assert_eq!(refused, Err(StatusCode::InvalidOperation));
    let st = session.inner.conn_state.lock().expect("conn_state");
    assert!(st.watcher.is_none());
}

/// A thread that cannot start is the link's error, the recipient is not on the list, and the next
/// link works.
#[test]
fn a_link_that_cannot_start_the_thread_fails_and_leaves_the_next_link_able() {
    let (session, peer) = client();
    let proxy = a_proxy(&session);
    let (tx, died) = mpsc::channel();
    let told = Arc::new(Told(Mutex::new(tx)));
    let weak: Weak<Told> = Arc::downgrade(&told);
    session
        .inner
        .shared
        .fail_spawn
        .store(true, Ordering::SeqCst);
    assert!(proxy.link_to_death(weak.clone()).is_err());
    assert!(session
        .inner
        .conn_state
        .lock()
        .expect("conn_state")
        .watcher
        .is_none());
    session
        .inner
        .shared
        .fail_spawn
        .store(false, Ordering::SeqCst);
    proxy.link_to_death(weak).expect("the next link");
    drop(peer);
    died.recv_timeout(WAIT).expect("the one recipient");
    assert!(died.recv_timeout(QUIET).is_err(), "not on the list twice");
}

/// Descriptors run out: the link says so. In a child, since the limit binds the whole process.
#[test]
fn a_link_with_no_descriptor_free_fails() {
    const CHILD: &str = "RSB_WATCH_NOFILE_CHILD";
    const NAME: &str = "rpc::session::peer_watch_tests::a_link_with_no_descriptor_free_fails";
    if std::env::var_os(CHILD).is_some() {
        let (session, _peer) = client();
        let proxy = a_proxy(&session);
        let (tx, _died) = mpsc::channel();
        let told = Arc::new(Told(Mutex::new(tx)));
        let weak: Weak<Told> = Arc::downgrade(&told);
        let nofile = rustix::process::Resource::Nofile;
        let limit = rustix::process::Rlimit {
            current: Some(0),
            maximum: rustix::process::getrlimit(nofile).maximum,
        };
        rustix::process::setrlimit(nofile, limit).expect("the limit");
        assert!(proxy.link_to_death(weak).is_err());
        return;
    }
    run_alone(CHILD, NAME);
}

/// In a child with `SIGPIPE` at `SIG_DFL`: a slot added once the watcher ended kills no one.
#[test]
fn a_slot_added_after_the_watcher_ended_raises_no_signal() {
    const CHILD: &str = "RSB_WATCH_SIGPIPE_CHILD";
    const NAME: &str =
        "rpc::session::peer_watch_tests::a_slot_added_after_the_watcher_ended_raises_no_signal";
    if std::env::var_os(CHILD).is_some() {
        // SAFETY: restores the default disposition; this child runs no other test.
        unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
        let (session, mut peer) = client();
        let (_proxy, _told, died) = linked(&session);
        // Held through the signal, so the watcher has ended with the session still up.
        let caller = {
            let inner = session.inner.clone();
            std::thread::spawn(move || {
                let mut counter = 40;
                let target = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
                inner
                    .client_transact(target, 1, &Parcel::new(), 0)
                    .map(drop)
            })
        };
        read_aosp_message(&mut peer).expect("the request");
        let other = another_connection(&session);
        drop(other);
        eventually("the job", || held_slots(&session) == 1);
        // Its thread is gone or going: this `add` finds a closed socket.
        std::thread::sleep(Duration::from_millis(50));
        let third = another_connection(&session);
        let reply = codec().encode_reply(&WireReply::default()).expect("reply");
        write_aosp_message(&mut peer, &reply).expect("the reply");
        caller.join().expect("the caller").expect("the call");
        died.recv_timeout(WAIT).expect("the recipient");
        drop(third);
        return;
    }
    run_alone(CHILD, NAME);
}

/// This test again in a process of its own, with `child` set.
fn run_alone(child: &str, name: &str) {
    use std::os::unix::process::ExitStatusExt;
    let out = std::process::Command::new(std::env::current_exe().expect("current_exe"))
        .args(["--exact", name, "--test-threads=1"])
        .env(child, "1")
        .output()
        .expect("spawn the child");
    assert_eq!(out.status.signal(), None, "a signal in the child");
    let stdout = String::from_utf8_lossy(&out.stdout);
    // Zero tests run would also exit 0: the filter must have found this test.
    assert!(
        out.status.success() && stdout.contains("1 passed"),
        "{:?}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A connection added after the link is watched from then on.
#[test]
fn a_connection_added_later_is_watched() {
    let (session, _first) = client();
    let (_proxy, _told, died) = linked(&session);
    let second = another_connection(&session);
    let polls = watcher_polls(&session);
    eventually("the wake", || polls.load(Ordering::SeqCst) == 1);
    drop(second);
    died.recv_timeout(WAIT).expect("the recipient");
}

/// A link on a session that ended starts nothing, and the obituary's latch answers it.
#[test]
fn a_link_on_an_ended_session_starts_no_thread() {
    let (session, _peer) = client();
    let proxy = a_proxy(&session);
    session.close_session();
    let (tx, _died) = mpsc::channel();
    let told = Arc::new(Told(Mutex::new(tx)));
    let weak: Weak<Told> = Arc::downgrade(&told);
    assert_eq!(proxy.link_to_death(weak), Err(StatusCode::DeadObject));
    let st = session.inner.conn_state.lock().expect("conn_state");
    assert!(st.watcher.is_none());
}

/// Frames that arrive for the calls of other threads do not wake it, and the signal ends it.
#[test]
fn frames_for_the_calls_do_not_wake_the_watcher() {
    let (session, mut peer) = client();
    let (_proxy, _told, died) = linked(&session);
    let polls = watcher_polls(&session);
    let mut counter = 40;
    let other = RpcAddress::unique(&mut counter, AddressSpace::Acceptor);
    let (frame, _) = codec().encode_dec_strong(&other, 1).expect("a frame");
    // Fewer than fill the socket, which nothing here reads.
    for _ in 0..100 {
        write_aosp_message(&mut peer, &frame).expect("a frame");
    }
    std::thread::sleep(QUIET);
    assert_eq!(polls.load(Ordering::SeqCst), 0);
    drop(peer);
    died.recv_timeout(WAIT).expect("the recipient");
    std::thread::sleep(QUIET);
    assert_eq!(polls.load(Ordering::SeqCst), 1, "it ended at the signal");
}

/// A link made while a loop was declared has no watcher, and the loop never came: one starts.
#[test]
fn a_failed_serve_spawn_watches_in_its_place() {
    let (session, peer) = client();
    let declared = &session.inner.shared.serve_declared;
    declared.store(1, Ordering::SeqCst);
    let (_proxy, _told, died) = linked(&session);
    declared.store(0, Ordering::SeqCst);
    session.inner.after_a_failed_serve_spawn();
    drop(peer);
    died.recv_timeout(WAIT).expect("the recipient");
}

/// The watcher told while a loop was declared, and the loop never came: its failure looks again.
#[test]
fn a_failed_serve_spawn_takes_up_a_signal_it_swallowed() {
    let (session, peer) = client();
    let (_proxy, _told, died) = linked(&session);
    let polls = watcher_polls(&session);
    let declared = &session.inner.shared.serve_declared;
    declared.store(1, Ordering::SeqCst);
    drop(peer);
    eventually("the signal", || polls.load(Ordering::SeqCst) == 1);
    assert!(died.recv_timeout(QUIET).is_err(), "it counted as a reader");
    declared.store(0, Ordering::SeqCst);
    session.inner.after_a_failed_serve_spawn();
    died.recv_timeout(WAIT).expect("the recipient");
}

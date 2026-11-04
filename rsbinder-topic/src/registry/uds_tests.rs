// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry over a Unix socket within one process: an RPC proxy is one in the same process too,
//! so a real registry, its publishers and its subscribers run with no binder device. The calls
//! that a resolver never makes wrongly are made by hand on the session.

use std::any::Any;
use std::collections::BTreeSet;
use std::io::{ErrorKind, IoSlice, Read, Write};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use rsbinder::rpc::transport::UnixTransport;
use rsbinder::rpc::{
    FileDescriptorTransportMode, PeerIdentity, RpcClientConfig, RpcResult, RpcServer,
    RpcServerHandle, RpcSession, RpcTransport,
};
use rsbinder::{BinderResult, Caller, ExceptionCode, Interface, Status, StatusCode, Strong};
use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};

use super::table::MAX_WATCHERS;
use super::uds::{ResolverOptions, SocketDir, UdsRegistry, UdsResolver};
use super::{Action, AllowAll, RegistryAccess};
use crate::aidl::ITopic::{BnTopic, ITopic};
use crate::aidl::ITopicAddressWatcher::{BnTopicAddressWatcher, ITopicAddressWatcher};
use crate::aidl::ITopicUdsRegistry::ITopicUdsRegistry;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::resolve::{Advertisement, OnResolved, Resolved, Watch};
use crate::uds::{self, Endpoint, Options, SessionGuard};
use crate::{
    Bytes, Closed, Disconnected, NamedState, NamedSubscriber, Publisher, PublisherPolicy,
    RecvError, Resolver, SubscriberPolicy,
};

const WAIT: Duration = Duration::from_secs(10);

fn eventually(what: &str, done: impl Fn() -> bool) {
    within(WAIT, what, done);
}

fn within(limit: Duration, what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + limit;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {limit:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn scratch(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    let dir = std::env::temp_dir().join(format!("rsb_topic_hub_{tag}_{pid}_{serial}"));
    std::fs::create_dir(&dir).unwrap();
    dir
}

/// A registry serving on a socket of its own, and what a test asks of it.
struct Fixture {
    path: PathBuf,
    registry: UdsRegistry,
    handle: Option<RpcServerHandle>,
}

impl Fixture {
    fn new(tag: &str, access: impl RegistryAccess + 'static) -> Self {
        Self::with_server(tag, access, |_| {})
    }

    /// With `change` made to the server's settings after [`UdsRegistry::serve`] made its own.
    fn with_server(
        tag: &str,
        access: impl RegistryAccess + 'static,
        change: impl FnOnce(&RpcServer),
    ) -> Self {
        let root = scratch(tag);
        let dir = SocketDir::open(&root.join("topic.d")).unwrap();
        let registry = UdsRegistry::new(dir, access);
        let path = root.join("topic.sock");
        let server = RpcServer::setup_unix_server(&path).unwrap();
        registry.serve(&server).unwrap();
        change(&server);
        let handle = Some(server.run_background().unwrap());
        Self {
            path,
            registry,
            handle,
        }
    }

    fn resolver(&self) -> UdsResolver {
        let endpoint = Endpoint::Path(self.path.clone());
        UdsResolver::connect(endpoint, &ResolverOptions::default()).unwrap()
    }

    /// A session of its own, for calls the resolver does not make.
    fn raw(&self) -> Raw {
        let endpoint = Endpoint::Path(self.path.clone());
        let session = uds::connect(endpoint, &Options::default()).unwrap();
        let registry = session.0.get_root().unwrap().into_interface().unwrap();
        Raw { registry, session }
    }

    fn stop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.stop_and_join().unwrap();
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop();
        if let Some(root) = self.path.parent() {
            let _ = std::fs::remove_dir_all(root);
        }
    }
}

struct Raw {
    registry: Strong<dyn ITopicUdsRegistry>,
    session: SessionGuard,
}

fn topic() -> (Publisher<Bytes>, Strong<dyn ITopic>) {
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    let topic = publisher.as_binder().into_interface().unwrap();
    (publisher, topic)
}

fn policy() -> SubscriberPolicy {
    SubscriberPolicy::default()
}

fn named(fixture: &Fixture, name: &str) -> NamedSubscriber<Bytes> {
    NamedSubscriber::subscribe_with(name, &policy(), &fixture.resolver()).unwrap()
}

fn code(status: rsbinder::Status) -> ExceptionCode {
    status.exception_code()
}

/// The connect through a descriptor a subscriber was given; a publisher gone is `ECONNREFUSED`.
fn connect_through(address: &OwnedFd) -> std::io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(format!("/proc/self/fd/{}", address.as_raw_fd()))
}

// ---- the directory ----

#[test]
fn a_socket_directory_is_made_private_and_what_a_crash_left_goes() {
    let root = scratch("dir");
    let path = root.join("topic.d");
    let dir = SocketDir::open(&path).unwrap();
    let stat = rustix::fs::stat(&path).unwrap();
    assert_eq!(stat.st_mode & 0o777, 0o700);
    drop(dir);
    drop(UnixListener::bind(path.join("7")).unwrap());
    let _dir = SocketDir::open(&path).unwrap();
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_directory_others_can_reach_a_symlink_or_a_file_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("dir_bad");
    let open = root.join("open.d");
    std::fs::create_dir(&open).unwrap();
    for mode in [0o770, 0o755, 0o711, 0o701, 0o500] {
        std::fs::set_permissions(&open, std::fs::Permissions::from_mode(mode)).unwrap();
        let refused = SocketDir::open(&open).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidData, "mode {mode:o}");
    }
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.join("link.d");
    std::os::unix::fs::symlink(&open, &link).unwrap();
    assert_eq!(
        SocketDir::open(&link).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    let file = root.join("file.d");
    std::fs::write(&file, b"").unwrap();
    assert_eq!(
        SocketDir::open(&file).unwrap_err().kind(),
        ErrorKind::InvalidData
    );
    std::fs::remove_dir_all(&root).unwrap();
}

/// A directory shared with something else, as `/etc/rsbinder/hub.d` is what `hub.sock` asks for.
#[test]
fn a_directory_holding_what_no_registry_made_is_refused_and_keeps_all_of_it() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("dir_shared");
    let path = root.join("hub.d");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    drop(UnixListener::bind(path.join("3")).unwrap());
    for stranger in ["10-local.toml", "12"] {
        std::fs::write(path.join(stranger), b"").unwrap();
        let refused = SocketDir::open(&path).unwrap_err();
        assert_eq!(refused.kind(), ErrorKind::InvalidData);
        assert!(refused.to_string().contains(stranger), "{refused}");
        assert!(path.join(stranger).exists());
        assert!(path.join("3").exists());
        std::fs::remove_file(path.join(stranger)).unwrap();
    }
    drop(UnixListener::bind(path.join("named")).unwrap());
    assert!(SocketDir::open(&path).is_err());
    assert!(path.join("3").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_directory_in_use_is_refused_and_keeps_what_is_in_it() {
    let root = scratch("dir_held");
    let path = root.join("topic.d");
    let held = SocketDir::open(&path).unwrap();
    drop(UnixListener::bind(path.join("3")).unwrap());
    let refused = SocketDir::open(&path).unwrap_err();
    assert_eq!(refused.kind(), ErrorKind::InvalidData);
    assert!(path.join("3").exists());
    drop(held);
    SocketDir::open(&path).unwrap();
    std::fs::remove_dir_all(&root).unwrap();
}

/// Nothing is left in the directory, and the socket's file has the mode a subscriber needs.
#[test]
fn an_allocated_socket_has_no_file_left_and_mode_0666() {
    let fixture = Fixture::new("unlinked", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    let dir = fixture.path.with_file_name("topic.d");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 0);
    let socket = OwnedFd::from(allocation.socket.unwrap());
    rustix::net::listen(&socket, 1).unwrap();
    raw.registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    let address = raw.registry.r#lookup("a.name").unwrap().socket.unwrap();
    let address = OwnedFd::from(address);
    let stat = rustix::fs::fstat(&address).unwrap();
    assert_eq!(stat.st_mode & 0o777, 0o666);
    let flags = rustix::fs::fcntl_getfl(&address).unwrap();
    assert!(flags.contains(rustix::fs::OFlags::PATH));
    connect_through(&address).unwrap();
}

// ---- allocate and commit ----

#[test]
fn an_allocation_is_found_by_nobody_until_its_commit() {
    let fixture = Fixture::new("pending", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    assert_eq!(fixture.registry.stats().pending, 1);
    assert!(raw.registry.r#lookup("a.name").unwrap().socket.is_none());
    assert!(raw.registry.r#list("").unwrap().is_empty());
    let watcher = BnTopicAddressWatcher::new_binder(Silent);
    let watched = raw.registry.r#watch("a.name", &watcher).unwrap();
    assert!(watched.socket.is_none());

    let generation = raw
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    assert_eq!(fixture.registry.stats().pending, 0);
    let found = raw.registry.r#lookup("a.name").unwrap();
    assert!(found.socket.is_some());
    assert_eq!(found.generation, generation);
    assert_eq!(raw.registry.r#list("").unwrap().len(), 1);
}

#[test]
fn the_session_that_ends_takes_its_allocation_and_its_names() {
    let fixture = Fixture::new("session_end", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let name = raw.registry.r#allocate("a.name", &topic).unwrap();
    raw.registry
        .r#commit("a.name", &topic, name.allocation)
        .unwrap();
    raw.registry.r#allocate("a.pending", &topic).unwrap();
    assert_eq!(fixture.registry.stats().pending, 1);
    assert_eq!(fixture.registry.stats().topics, 1);
    drop(raw.session);
    eventually("the names to go", || {
        let stats = fixture.registry.stats();
        stats.pending == 0 && stats.topics == 0
    });
}

/// Who holds an allocation is told by the proxy of its binder, which no other session can make.
#[test]
fn another_session_cannot_commit_or_withdraw_an_allocation() {
    let fixture = Fixture::new("forged", AllowAll);
    let owner = fixture.raw();
    let other = fixture.raw();
    let (_publisher, topic) = topic();
    let allocation = owner.registry.r#allocate("a.name", &topic).unwrap();
    let forged = other
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap_err();
    assert_eq!(code(forged), ExceptionCode::IllegalState);
    owner
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    other
        .registry
        .r#withdraw("a.name", &topic, allocation.allocation)
        .unwrap();
    // A oneway call after it on the same session has been served.
    other.registry.r#list("").unwrap();
    assert_eq!(fixture.registry.stats().topics, 1);
}

#[test]
fn a_replaced_or_withdrawn_allocation_is_not_committed() {
    let fixture = Fixture::new("replaced", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let first = raw.registry.r#allocate("a.name", &topic).unwrap();
    let second = raw.registry.r#allocate("a.name", &topic).unwrap();
    assert_eq!(fixture.registry.stats().pending, 1);
    let refused = raw
        .registry
        .r#commit("a.name", &topic, first.allocation)
        .unwrap_err();
    assert_eq!(code(refused), ExceptionCode::IllegalState);
    // The one that replaced it was left alone by the failed commit of the first.
    assert_eq!(fixture.registry.stats().pending, 1);
    raw.registry
        .r#withdraw("a.name", &topic, second.allocation)
        .unwrap();
    raw.registry.r#list("").unwrap();
    assert_eq!(fixture.registry.stats().pending, 0);
    let refused = raw
        .registry
        .r#commit("a.name", &topic, second.allocation)
        .unwrap_err();
    assert_eq!(code(refused), ExceptionCode::IllegalState);
}

/// Two publishers of a uid: the later commit wins, whichever allocated first.
#[test]
fn commits_in_the_reverse_order_of_their_allocations_order_the_generations() {
    let fixture = Fixture::new("reversed", AllowAll);
    let a = fixture.raw();
    let b = fixture.raw();
    let (_pa, topic_a) = topic();
    let (_pb, topic_b) = topic();
    let first = a.registry.r#allocate("a.name", &topic_a).unwrap();
    let second = b.registry.r#allocate("a.name", &topic_b).unwrap();
    let g2 = b
        .registry
        .r#commit("a.name", &topic_b, second.allocation)
        .unwrap();
    let g1 = a
        .registry
        .r#commit("a.name", &topic_a, first.allocation)
        .unwrap();
    assert!(g1 > g2);
    let found = a.registry.r#lookup("a.name").unwrap();
    assert_eq!(found.generation, g1);
}

/// Once told, refuses to advertise any name, as a reload can between an allocation and its commit.
#[derive(Default)]
struct ReloadedAway(Mutex<bool>);

impl RegistryAccess for Arc<ReloadedAway> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        action != Action::Advertise || !*self.0.lock().unwrap()
    }
}

#[test]
fn a_commit_refused_leaves_nothing_and_allocating_again_works() {
    let access = Arc::new(ReloadedAway::default());
    let fixture = Fixture::new("refused", access.clone());
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    *access.0.lock().unwrap() = true;
    let refused = raw
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap_err();
    assert_eq!(code(refused), ExceptionCode::Security);
    assert_eq!(fixture.registry.stats().pending, 0);
    assert_eq!(fixture.registry.stats().topics, 0);
    *access.0.lock().unwrap() = false;
    let again = raw.registry.r#allocate("a.name", &topic).unwrap();
    raw.registry
        .r#commit("a.name", &topic, again.allocation)
        .unwrap();
    assert_eq!(fixture.registry.stats().topics, 1);
}

/// A oneway withdraw handled late names its own allocation, so it takes no newer one back.
#[test]
fn a_withdraw_of_an_old_allocation_leaves_the_new_one() {
    let fixture = Fixture::new("late", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let old = raw.registry.r#allocate("a.name", &topic).unwrap();
    raw.registry
        .r#commit("a.name", &topic, old.allocation)
        .unwrap();
    let new = raw.registry.r#allocate("a.name", &topic).unwrap();
    raw.registry
        .r#commit("a.name", &topic, new.allocation)
        .unwrap();
    raw.registry
        .r#withdraw("a.name", &topic, old.allocation)
        .unwrap();
    raw.registry.r#list("").unwrap();
    assert!(raw.registry.r#lookup("a.name").unwrap().socket.is_some());
    raw.registry
        .r#withdraw("a.name", &topic, new.allocation)
        .unwrap();
    raw.registry.r#list("").unwrap();
    assert!(raw.registry.r#lookup("a.name").unwrap().socket.is_none());
}

/// A commit of another uid: the table's own judgement, with the uid of a session faked by a key.
#[test]
fn the_table_judges_who_holds_a_name_at_the_commit_again() {
    use super::table::{Key, Links, Refused, Table};

    #[derive(Clone, PartialEq, Debug)]
    struct K(u64, u64);
    impl Key for K {
        fn handle(&self) -> Option<u64> {
            Some(self.0)
        }
    }
    #[derive(Default)]
    struct NoLinks;
    impl Links<K, K> for NoLinks {
        fn link_topic(&mut self, _: u64, _: &K) -> rsbinder::BinderResult<()> {
            Ok(())
        }
        fn link_watcher(&mut self, _: u64, _: &K) -> rsbinder::BinderResult<()> {
            Ok(())
        }
        fn unlink(&mut self, _: u64) {}
    }
    let now = Instant::now();
    let mut table = Table::<K, K>::new();
    let mut links = NoLinks;
    table.allocate(1, "n", K(1, 1), false, &mut links).unwrap();
    table.allocate(2, "n", K(2, 2), false, &mut links).unwrap();
    table
        .commit((1, 10), "n", &K(1, 1), false, now, &mut links)
        .unwrap();
    assert_eq!(
        table
            .commit((2, 20), "n", &K(2, 2), false, now, &mut links)
            .err(),
        Some(Refused::Taken)
    );
    // Nothing of the refused one is left, and the same handle's new key takes the name's place.
    assert_eq!(table.counts().pending, 0);
    assert_eq!(
        table.allocate(2, "n", K(2, 3), false, &mut links).err(),
        Some(Refused::Taken)
    );
    table.allocate(1, "n", K(1, 4), false, &mut links).unwrap();
    table
        .commit((1, 10), "n", &K(1, 4), false, now, &mut links)
        .unwrap();
    assert_eq!(table.linked(), [(1, 1)].into());
    table.check();
    table.forget(1);
    assert_eq!(table.counts().topics, 0);
    table.check();
}

// ---- the policy ----

struct OnlyName(&'static str);

impl RegistryAccess for OnlyName {
    fn allows(&self, _: Action, name: &str, _: &Caller) -> bool {
        name == self.0 || name.is_empty()
    }
}

#[test]
fn the_policy_refuses_a_name_it_does_not_allow() {
    let fixture = Fixture::new("policy", OnlyName("allowed.name"));
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let refused = raw.registry.r#allocate("other.name", &topic).unwrap_err();
    assert_eq!(code(refused), ExceptionCode::Security);
    assert_eq!(fixture.registry.stats().pending, 0);

    let watcher = BnTopicAddressWatcher::new_binder(Silent);
    let refused = raw.registry.r#watch("other.name", &watcher).unwrap_err();
    assert_eq!(code(refused), ExceptionCode::Security);

    let answer = raw.registry.r#lookup("other.name").unwrap();
    assert!(answer.socket.is_none());
    assert_eq!(answer.generation, i64::MIN);

    let bad = raw.registry.r#allocate("no spaces", &topic).unwrap_err();
    assert_eq!(code(bad), ExceptionCode::IllegalArgument);
}

/// Allows a subscriber until told, which a reload can do to a watch already made.
#[derive(Default)]
struct WatchersGoAway(Mutex<bool>, AtomicU32);

impl RegistryAccess for Arc<WatchersGoAway> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        let allowed = action != Action::Subscribe || !*self.0.lock().unwrap();
        if !allowed {
            self.1.fetch_add(1, Ordering::Relaxed);
        }
        allowed
    }
}

/// Counts the notices of the watch.
#[derive(Default, Clone)]
struct Counted(Arc<Mutex<Vec<i64>>>);

impl Interface for Counted {}

impl ITopicAddressWatcher for Counted {
    fn r#onAdvertised(&self, _: &str, generation: i64) -> rsbinder::BinderResult<()> {
        self.0.lock().unwrap().push(generation);
        Ok(())
    }
}

struct Silent;

impl Interface for Silent {}

impl ITopicAddressWatcher for Silent {
    fn r#onAdvertised(&self, _: &str, _: i64) -> rsbinder::BinderResult<()> {
        Ok(())
    }
}

#[test]
fn a_watcher_that_lost_the_right_is_sent_no_notice() {
    let access = Arc::new(WatchersGoAway::default());
    let fixture = Fixture::new("reload", access.clone());
    let raw = fixture.raw();
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch("a.name", &watcher).unwrap();

    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    let generation = raw
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    eventually("the notice", || !told.0.lock().unwrap().is_empty());
    assert_eq!(*told.0.lock().unwrap(), [generation]);

    *access.0.lock().unwrap() = true;
    let again = raw.registry.r#allocate("a.name", &topic).unwrap();
    raw.registry
        .r#commit("a.name", &topic, again.allocation)
        .unwrap();
    // `commit` judges and sends on its thread: what it would have sent, it has by now.
    assert_eq!(fixture.registry.stats().notices_sent, 1);
    assert_eq!(told.0.lock().unwrap().len(), 1);
}

// ---- the socket ----

/// The registry's own copy of a bound socket would answer connects for a dead publisher.
#[test]
fn the_registry_keeps_no_copy_of_a_socket() {
    let fixture = Fixture::new("no_copy", AllowAll);
    let raw = fixture.raw();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    let id = allocation.allocation;
    let socket = OwnedFd::from(allocation.socket.unwrap());
    rustix::net::listen(&socket, 8).unwrap();
    raw.registry.r#commit("a.name", &topic, id).unwrap();
    let address = OwnedFd::from(raw.registry.r#lookup("a.name").unwrap().socket.unwrap());
    connect_through(&address).unwrap();
    drop(socket);
    let refused = connect_through(&address).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(libc::ECONNREFUSED));
}

// ---- the resolver ----

#[test]
fn a_name_subscriber_reaches_a_publisher_through_the_registry() {
    let fixture = Fixture::new("named", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let mut subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    assert_eq!(fixture.registry.stats().topics, 1);
}

/// Either order: the watch hears of the name when it comes.
#[test]
fn a_name_subscriber_that_comes_first_hears_of_the_publisher() {
    let fixture = Fixture::new("first", AllowAll);
    let mut subscriber = named(&fixture, "a.name");
    assert!(!subscriber.wait_connected(Duration::from_millis(50)));
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![2])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![2])));
}

/// Drained, then one `Replaced`, then the new publisher from its first record.
#[test]
fn a_name_given_to_another_publisher_replaces_once() {
    let fixture = Fixture::new("replace", AllowAll);
    let resolver = fixture.resolver();
    let first =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let mut subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));
    first.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));

    first.publish(&Bytes(vec![2])).unwrap();
    let second =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    second.publish(&Bytes(vec![3])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![2])));
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![3])));
    drop(first);
    second.publish(&Bytes(vec![4])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![4])));
}

/// A publisher that closes ends with its end record, and its name is taken back.
#[test]
fn a_closed_publisher_ends_its_subscribers_and_takes_its_name_back() {
    let fixture = Fixture::new("close", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let mut subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![1])).unwrap();
    drop(publisher);
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    eventually("the name to go", || fixture.registry.stats().topics == 0);
}

#[test]
fn a_registry_that_ends_is_the_resolvers_death_and_connections_go_on() {
    let mut fixture = Fixture::new("host", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let mut subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));
    fixture.stop();
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    drop(publisher);
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::ResolverDied))
    );
}

/// Two names on one resolver: one advertisement's drop leaves the session and the other name.
#[test]
fn dropping_one_advertisement_leaves_the_resolvers_other_name_and_watch() {
    let fixture = Fixture::new("two", AllowAll);
    let resolver = fixture.resolver();
    let keep =
        Publisher::<Bytes>::advertise_with("keep", &PublisherPolicy::default(), &resolver).unwrap();
    let gone =
        Publisher::<Bytes>::advertise_with("gone", &PublisherPolicy::default(), &resolver).unwrap();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("keep", &policy(), &resolver).unwrap();
    assert!(subscriber.wait_connected(WAIT));
    drop(gone);
    eventually("the name to go", || fixture.registry.stats().topics == 1);
    keep.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    let again =
        Publisher::<Bytes>::advertise_with("gone", &PublisherPolicy::default(), &resolver).unwrap();
    assert_eq!(fixture.registry.stats().topics, 2);
    drop(again);
}

/// `a` → `b` → `a`: the first withdraw of `a` is older than the second allocation of it.
#[test]
fn a_publisher_moved_between_names_and_back_keeps_the_last() {
    let fixture = Fixture::new("aba", AllowAll);
    let resolver = fixture.resolver();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    publisher.advertise_on("a", &resolver).unwrap();
    publisher.advertise_on("b", &resolver).unwrap();
    publisher.advertise_on("a", &resolver).unwrap();
    let listed = || {
        let names = resolver.list("").unwrap();
        names.into_iter().map(|l| l.name).collect::<BTreeSet<_>>()
    };
    eventually("only the last name", || listed() == ["a".to_owned()].into());
    assert_eq!(fixture.registry.stats().topics, 1);
}

#[test]
fn a_name_of_a_publisher_advertised_to_one_host_twice_is_asked_once() {
    let fixture = Fixture::new("same_host", AllowAll);
    let resolver = fixture.resolver();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    publisher.advertise_on("a", &resolver).unwrap();
    let generation = fixture.raw().registry.r#lookup("a").unwrap().generation;
    publisher.advertise_on("a", &resolver).unwrap();
    assert_eq!(
        fixture.raw().registry.r#lookup("a").unwrap().generation,
        generation
    );
    // Another resolver is another host binder: it replaces its own advertisement.
    let other = fixture.resolver();
    publisher.advertise_on("a", &other).unwrap();
    assert!(fixture.raw().registry.r#lookup("a").unwrap().generation > generation);
    drop(resolver);
    assert_eq!(fixture.registry.stats().topics, 1);
}

#[test]
fn a_remote_binder_is_not_advertised() {
    let fixture = Fixture::new("remote", AllowAll);
    let resolver = fixture.resolver();
    let (publisher, _) = topic();
    let path = fixture.path.with_file_name("topic2.sock");
    let server = RpcServer::setup_unix_server(&path).unwrap();
    let _handle = uds::serve(&server, publisher.as_binder()).unwrap();
    let session = uds::connect(Endpoint::Path(path), &Options::default()).unwrap();
    let remote = session.0.get_root().unwrap();
    let refused = resolver.advertise("a.name", remote).err().unwrap();
    assert_eq!(refused.transaction_error(), StatusCode::BadValue);
    assert_eq!(fixture.registry.stats().pending, 0);
}

/// A new socket is not a new publisher: what the subscriber read is not read again.
#[test]
fn a_publisher_given_a_new_socket_is_the_publisher_a_subscriber_read() {
    let fixture = Fixture::new("resocket", AllowAll);
    let policy = SubscriberPolicy {
        start: crate::Start::Oldest,
        ..SubscriberPolicy::default()
    };
    let first = fixture.resolver();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    publisher.advertise_on("a.name", &first).unwrap();
    publisher.publish(&Bytes(vec![1])).unwrap();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy, &fixture.resolver()).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    let second = fixture.resolver();
    publisher.advertise_on("a.name", &second).unwrap();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![2])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![2])));
    // Another publisher starts where the policy says: its first record is not missed.
    let other = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    other.publish(&Bytes(vec![3])).unwrap();
    other.advertise_on("a.name", &first).unwrap();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Replaced))
    );
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![3])));
}

#[test]
fn a_resolver_subscribes_to_the_topic_a_name_has_now() {
    let fixture = Fixture::new("plain", AllowAll);
    let resolver = fixture.resolver();
    let refused = resolver.subscribe::<Bytes>("a.name", &policy());
    assert_eq!(
        refused.err().map(|status| status.transaction_error()),
        Some(StatusCode::NameNotFound)
    );
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let mut subscriber = fixture
        .resolver()
        .subscribe::<Bytes>("a.name", &policy())
        .unwrap();
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
}

/// Lets a name be allocated and refuses its commit, the next time the policy is asked of it.
struct RefusesCommitOf(&'static str, AtomicU32);

impl RegistryAccess for Arc<RefusesCommitOf> {
    fn allows(&self, action: Action, name: &str, _: &Caller) -> bool {
        action != Action::Advertise || name != self.0 || self.1.fetch_add(1, Ordering::Relaxed) == 0
    }
}

/// A commit the resolver loses keeps what the publisher had, and stops the server it made.
#[test]
fn a_commit_a_policy_refuses_leaves_the_publisher_its_name() {
    let access = Arc::new(RefusesCommitOf("b.name", AtomicU32::new(0)));
    let fixture = Fixture::new("keep_name", access.clone());
    let resolver = fixture.resolver();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    publisher.advertise_on("a.name", &resolver).unwrap();
    let raw = fixture.raw();
    let held = raw.registry.r#lookup("a.name").unwrap();
    let refused = publisher.advertise_on("b.name", &resolver).err().unwrap();
    assert_eq!(code(refused), ExceptionCode::Security);
    assert_eq!(
        access.1.load(Ordering::Relaxed),
        2,
        "the allocate, then the commit"
    );
    let found = raw.registry.r#lookup("a.name").unwrap();
    assert_eq!(found.generation, held.generation);
    assert!(found.socket.is_some());
    assert_eq!(fixture.registry.stats().pending, 0);
    let address = fixture.registry.last_address().unwrap();
    let stopped = connect_through(&address).unwrap_err();
    assert_eq!(stopped.raw_os_error(), Some(libc::ECONNREFUSED));
}

#[test]
fn the_state_of_a_name_with_no_publisher_says_so() {
    let fixture = Fixture::new("state", AllowAll);
    let subscriber = named(&fixture, "no.publisher");
    assert_eq!(subscriber.state(), NamedState::Waiting);
}

#[test]
fn a_publisher_of_an_unexpected_uid_is_not_subscribed_to() {
    let fixture = Fixture::new("uid", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let own = rustix::process::geteuid().as_raw();
    let options = ResolverOptions {
        expect_publisher_uid: Some(own + 1),
        ..ResolverOptions::default()
    };
    let endpoint = Endpoint::Path(fixture.path.clone());
    let picky = UdsResolver::connect(endpoint, &options).unwrap();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &picky).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(200)));
    publisher.publish(&Bytes(vec![1])).unwrap();
    match subscriber.recv_timeout(WAIT) {
        Err(RecvError::Disconnected(Disconnected::Rejected(status))) => {
            assert_eq!(status.exception_code(), ExceptionCode::Security);
        }
        other => panic!("not told of the refusal: {other:?}"),
    }
    assert!(matches!(
        subscriber.state(),
        NamedState::Refused(status) if status.exception_code() == ExceptionCode::Security
    ));
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(50)),
        Err(RecvError::Timeout)
    );
}

/// A resolver that gives up on a publisher tells the subscriber once, and a binder ends that.
#[test]
fn a_publisher_given_up_on_is_a_refusal_until_a_binder_comes() {
    struct GivesUp(Mutex<Option<OnResolved>>);

    impl Resolver for GivesUp {
        fn advertise(&self, _: &str, _: rsbinder::SIBinder) -> BinderResult<Advertisement> {
            Ok(Advertisement::none())
        }

        fn watch(
            &self,
            _: &str,
            _: Box<dyn Fn(rsbinder::SIBinder) + Send + Sync>,
        ) -> BinderResult<Watch> {
            Ok(Watch::new(()))
        }

        fn watch_resolved(&self, _: &str, on_resolved: OnResolved) -> BinderResult<Watch> {
            *self.0.lock().unwrap() = Some(on_resolved);
            Ok(Watch::new(()))
        }
    }

    let resolver = GivesUp(Mutex::default());
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &resolver).unwrap();
    let tell = resolver.0.lock().unwrap().take().unwrap();
    let refusal = || Err(Status::from((ExceptionCode::Security, "not that uid")));
    tell(refusal());
    tell(refusal());
    assert!(matches!(subscriber.state(), NamedState::Refused(_)));
    assert!(matches!(
        subscriber.try_recv(),
        Err(RecvError::Disconnected(Disconnected::Rejected(_)))
    ));
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    let (_publisher, topic) = topic();
    tell(Ok(Resolved::new(topic.as_binder())));
    assert!(subscriber.wait_connected(WAIT));
    assert_eq!(subscriber.state(), NamedState::Connected);
}

/// A session that ended is no answer for its generation: the publisher lives, and is reached anew.
#[test]
fn a_session_that_ended_is_made_again_for_the_same_generation() {
    let fixture = Fixture::new("session_ended", AllowAll);
    let raw = fixture.raw();
    let (_publisher, _serving, address, generation) = served(&raw, "a.name");
    let resolver = fixture.resolver();
    let watch = resolver.reconciler("a.name", Box::new(|_| {}));
    let first = watch.dial(&address, generation).unwrap().unwrap();
    let again = watch.dial(&address, generation).unwrap().unwrap();
    assert_eq!(again.binder(), first.binder());
    let (first_binder, session) = first.into_parts();
    let session = session.unwrap().downcast::<SessionGuard>().unwrap();
    session.0.close_session();
    let next = watch.dial(&address, generation).unwrap().unwrap();
    assert_ne!(next.binder(), &first_binder);
    next.binder().ping_binder().unwrap();
    watch.end();
}

/// Its session ending is what the subscriber sees: the name and the publisher stay as they were.
#[test]
fn a_subscriber_whose_session_ended_connects_again_with_no_new_generation() {
    let fixture = Fixture::new("reconnect", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = SessionsSeen {
        resolver: fixture.resolver(),
        seen: seen.clone(),
    };
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &spy).unwrap();
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    let session = seen.lock().unwrap()[0].upgrade().unwrap();
    let guard = session.downcast::<SessionGuard>().unwrap();
    guard.0.close_session();
    drop(guard);
    assert!(matches!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(_)))
    ));
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![2])).unwrap();
    // A session's end reads as the publisher's death, which forgets it: from the oldest again.
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![2])));
}

/// A notice could reach neither: no incoming connection to call on, or no descriptor to take.
#[test]
fn a_watch_of_a_session_that_cannot_be_told_is_refused() {
    let fixture = Fixture::new("no_callbacks", AllowAll);
    let config = || RpcClientConfig::unix(fixture.path.clone(), uds::WIRE);
    let no_incoming = config().fd_mode(FileDescriptorTransportMode::Unix);
    let no_fds = config().incoming_connections(1);
    for config in [no_incoming, no_fds] {
        let session = RpcSession::setup_client_android13plus_with_config(config).unwrap();
        let session = SessionGuard(session);
        let registry: Strong<dyn ITopicUdsRegistry> =
            session.0.get_root().unwrap().into_interface().unwrap();
        let watcher = BnTopicAddressWatcher::new_binder(Silent);
        let refused = registry.r#watch("a.name", &watcher).err().unwrap();
        assert_eq!(refused.transaction_error(), StatusCode::InvalidOperation);
        assert_eq!(fixture.registry.stats().watchers, 0);
    }
}

/// What a client sends with a frame it never finishes is closed with the read: no call takes one.
#[test]
fn a_descriptor_a_client_sends_is_closed_with_the_read() {
    let fixture = Fixture::new("sent_fd", AllowAll);
    let mut client = UnixStream::connect(&fixture.path).unwrap();
    // The android-13+ connection header (version 2, `Unix`, a new session), then its "cci".
    let mut hello = [0u8; 24];
    hello[0] = 2;
    hello[5] = 1;
    hello[16..19].copy_from_slice(b"cci");
    client.write_all(&hello).unwrap();
    client.set_read_timeout(Some(WAIT)).unwrap();
    client.read_exact(&mut [0u8; 8]).unwrap();

    let (mut kept, sent) = UnixStream::pair().unwrap();
    kept.set_read_timeout(Some(WAIT)).unwrap();
    let fds = [sent.as_fd()];
    let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(1))];
    let mut ancillary = SendAncillaryBuffer::new(&mut space);
    assert!(ancillary.push(SendAncillaryMessage::ScmRights(&fds)));
    // One byte of a header: a read that took the descriptor would hold it for the rest.
    let frame = [IoSlice::new(&[0])];
    rustix::net::sendmsg(&client, &frame, &mut ancillary, SendFlags::empty()).unwrap();
    drop(sent);
    assert_eq!(kept.read(&mut [0]).unwrap(), 0, "the host holds it");
}

#[test]
fn a_host_of_an_unexpected_uid_is_refused() {
    let fixture = Fixture::new("host_uid", AllowAll);
    let own = rustix::process::geteuid().as_raw();
    let options = ResolverOptions {
        expect_host_uid: Some(own + 1),
        ..ResolverOptions::default()
    };
    let endpoint = Endpoint::Path(fixture.path.clone());
    let refused = UdsResolver::connect(endpoint, &options).err().unwrap();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
}

/// A session for each generation, and one for repeats of one, a lookup's or a notice's.
#[test]
fn a_lookup_and_a_notice_of_one_generation_share_a_binder() {
    let fixture = Fixture::new("same_gen", AllowAll);
    let resolver = fixture.resolver();
    let _publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let told = Arc::new(Mutex::new(Vec::new()));
    let keep = told.clone();
    let watch = resolver
        .watch(
            "a.name",
            Box::new(move |binder| keep.lock().unwrap().push(binder)),
        )
        .unwrap();
    eventually("the reply's call", || !told.lock().unwrap().is_empty());
    let lookup = watch.lookup().unwrap();
    let looked = lookup().unwrap().unwrap();
    assert_eq!(looked.binder(), &told.lock().unwrap()[0]);
    assert_eq!(told.lock().unwrap().len(), 1);
}

// ---- a publisher or a watcher that stops ----

/// A publisher given a name on `raw`'s session: it, what serves it, the address and generation.
fn served(raw: &Raw, name: &str) -> (Publisher<Bytes>, RpcServerHandle, OwnedFd, i64) {
    let (publisher, topic) = topic();
    let allocation = raw.registry.r#allocate(name, &topic).unwrap();
    let socket = OwnedFd::from(allocation.socket.unwrap());
    let server = RpcServer::setup_unix_server_from_bound_fd(socket).unwrap();
    let serving = uds::serve(&server, publisher.as_binder()).unwrap();
    let generation = raw
        .registry
        .r#commit(name, &topic, allocation.allocation)
        .unwrap();
    let address = raw.registry.r#lookup(name).unwrap().socket.unwrap();
    (publisher, serving, OwnedFd::from(address), generation)
}

/// A socket listening with no one to accept is a publisher that stopped, until it is served.
#[test]
fn a_publisher_that_answers_only_later_is_reached_with_no_newer_generation() {
    let fixture = Fixture::new("late_answer", AllowAll);
    let raw = fixture.raw();
    let (publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    let socket = OwnedFd::from(allocation.socket.unwrap());
    rustix::net::listen(&socket, 8).unwrap();
    raw.registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    let options = ResolverOptions {
        timeout: Some(Duration::from_millis(100)),
        ..ResolverOptions::default()
    };
    // Tried again: the deadline meant for the publisher limits this connect too, under load.
    let resolver = (0..100)
        .find_map(|_| UdsResolver::connect(Endpoint::Path(fixture.path.clone()), &options).ok())
        .expect("a connect to the host within 100 tries");
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &resolver).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(300)));

    let server = RpcServer::setup_unix_server_from_bound_fd(socket).unwrap();
    let _serving = uds::serve(&server, publisher.as_binder()).unwrap();
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
}

/// A payload of another type, which a publisher of `Bytes` refuses for good.
#[derive(Debug, PartialEq)]
struct Word;

impl crate::Payload for Word {
    fn type_name() -> &'static str {
        "rsbinder.topic.test.Word"
    }

    fn encode(&self, _: &mut crate::Encoding<'_>) -> rsbinder::Result<()> {
        Ok(())
    }

    fn decode(_: &mut crate::Record<'_>) -> rsbinder::Result<Self> {
        Ok(Self)
    }
}

/// A subscriber a publisher refuses holds none of the connections the publisher allows.
#[test]
fn a_refused_subscriber_leaves_the_publishers_connection_to_another() {
    let fixture = Fixture::new("refused_lets_go", AllowAll);
    let options = ResolverOptions {
        publisher_connections: Some(2),
        ..ResolverOptions::default()
    };
    let endpoint = Endpoint::Path(fixture.path.clone());
    let advertising = UdsResolver::connect(endpoint, &options).unwrap();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &advertising)
            .unwrap();
    let resolver = fixture.resolver();
    let mut refused =
        NamedSubscriber::<Word>::subscribe_with("a.name", &policy(), &resolver).unwrap();
    let told = refused.recv_timeout(WAIT);
    match told {
        Err(RecvError::Disconnected(Disconnected::Rejected(status))) => {
            assert_eq!(status.transaction_error(), StatusCode::BadType);
        }
        other => panic!("not told of the refusal: {other:?}"),
    }
    assert!(matches!(refused.state(), NamedState::Refused(_)));

    let mut other =
        NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &resolver).unwrap();
    assert!(other.wait_connected(WAIT), "the refused one holds the room");
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(other.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    assert!(matches!(refused.state(), NamedState::Refused(_)));
}

/// A notice brings no descriptor: with none free the session lives, and the watch asks once one is.
#[test]
fn a_process_with_no_descriptor_free_keeps_its_session_and_connects_later() {
    /// The host's socket, set in the child, which is the process whose table is full.
    const HOST: &str = "RSB_TOPIC_TEST_FULL_TABLE_HOST";
    if let Some(host) = std::env::var_os(HOST) {
        let host = PathBuf::from(host);
        let endpoint = Endpoint::Path(host.clone());
        let resolver = UdsResolver::connect(endpoint, &ResolverOptions::default()).unwrap();
        let subscriber =
            NamedSubscriber::<Bytes>::subscribe_with("a.full", &policy(), &resolver).unwrap();
        let mut full = std::fs::File::create(host.with_file_name("full")).unwrap();
        let limit = rustix::process::Rlimit {
            current: Some(512),
            maximum: rustix::process::getrlimit(rustix::process::Resource::Nofile).maximum,
        };
        rustix::process::setrlimit(rustix::process::Resource::Nofile, limit).unwrap();
        let file = std::fs::File::open("/dev/null").unwrap();
        let mut filling = Vec::new();
        while let Ok(next) = file.try_clone() {
            filling.push(next);
        }
        full.write_all(b"full").unwrap();
        // The notice comes, and the watch's rounds find no descriptor free.
        assert!(!subscriber.wait_connected(Duration::from_secs(2)));
        resolver.list("").expect("the session goes on");
        drop(filling);
        assert!(subscriber.wait_connected(WAIT));
        return;
    }
    let fixture = Fixture::new("no_descriptor", AllowAll);
    let raw = fixture.raw();
    let (publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.full", &topic).unwrap();
    let socket = OwnedFd::from(allocation.socket.unwrap());
    let server = RpcServer::setup_unix_server_from_bound_fd(socket).unwrap();
    let _serving = uds::serve(&server, publisher.as_binder()).unwrap();

    let module = module_path!().split_once("::").unwrap().1;
    let test =
        format!("{module}::a_process_with_no_descriptor_free_keeps_its_session_and_connects_later");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([test.as_str(), "--exact"])
        .env(HOST, &fixture.path)
        .spawn()
        .unwrap();
    let full = fixture.path.with_file_name("full");
    eventually("the child's table full", || {
        std::fs::read(&full).is_ok_and(|read| !read.is_empty())
    });
    raw.registry
        .r#commit("a.full", &topic, allocation.allocation)
        .unwrap();
    assert_eq!(fixture.registry.stats().notices_sent, 1);
    assert!(child.wait().unwrap().success());
}

/// All watches of a resolver share one host watch per name, which ends with the name's last.
#[test]
fn the_watches_of_a_name_share_a_host_watch_that_ends_with_the_last() {
    let fixture = Fixture::new("shared_watch", AllowAll);
    let resolver = fixture.resolver();
    let watch = |name| NamedSubscriber::<Bytes>::subscribe_with(name, &policy(), &resolver);
    let first = watch("a.name").unwrap();
    let mut second = watch("a.name").unwrap();
    let other = watch("a.other").unwrap();
    assert_eq!(fixture.registry.stats().watchers, 2);

    drop(first);
    let raw = fixture.raw();
    let (publisher, _serving, _address, _generation) = served(&raw, "a.name");
    assert!(second.wait_connected(WAIT), "the other watch of the name");
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(second.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    assert_eq!(fixture.registry.stats().watchers, 2);

    drop(second);
    eventually("the name's last watch", || {
        fixture.registry.stats().watchers == 1
    });
    drop(other);
    eventually("the other name's", || {
        fixture.registry.stats().watchers == 0
    });
}

/// A name watched again as its last watch ends keeps a host watch, whichever call comes first.
#[test]
fn a_name_watched_again_right_after_its_last_watch_hears_of_a_change() {
    let fixture = Fixture::new("watched_again", AllowAll);
    let resolver = fixture.resolver();
    let watch = || NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &resolver);
    let mut subscriber = watch().unwrap();
    for _ in 0..50 {
        drop(subscriber);
        subscriber = watch().unwrap();
    }
    let raw = fixture.raw();
    let (_publisher, _serving, _address, _generation) = served(&raw, "a.name");
    assert!(subscriber.wait_connected(WAIT));
}

/// A session may have one watch address, and not as a count of its names: many take one address.
#[test]
fn a_resolver_watches_many_names_on_one_address_and_no_session_ends() {
    let fixture = Fixture::new("many_names", AllowAll);
    let resolver = fixture.resolver();
    let names: Vec<String> = (0..20).map(|n| format!("a.name{n}")).collect();
    let mut subscribers: Vec<NamedSubscriber<Bytes>> = names
        .iter()
        .map(|name| NamedSubscriber::subscribe_with(name, &policy(), &resolver).unwrap())
        .collect();
    let raw = fixture.raw();
    let mut publishers = Vec::new();
    for name in &names {
        publishers.push(served(&raw, name));
    }
    for subscriber in &mut subscribers {
        assert!(subscriber.wait_connected(WAIT));
    }
    assert_eq!(fixture.registry.stats().watchers, names.len());
}

/// A callback that panics stops one call: what was given meanwhile and later still connect.
#[test]
fn a_panic_in_the_callback_leaves_the_watch_able_to_connect() {
    let fixture = Fixture::new("panicking_callback", AllowAll);
    let raw = fixture.raw();
    let (_publisher, _serving, address, generation) = served(&raw, "a.name");
    let told = Arc::new(Mutex::new(Vec::new()));
    let keep = told.clone();
    let (in_the_callback, is_in_it) = mpsc::channel();
    let (panic_now, panic_is_due) = mpsc::channel::<()>();
    let waits = Mutex::new(Some((in_the_callback, panic_is_due)));
    let resolver = fixture.resolver();
    let watch = resolver.reconciler(
        "a.name",
        Box::new(move |resolved: BinderResult<Resolved>| {
            keep.lock().unwrap().push(resolved.unwrap().into_binder());
            if let Some((in_the_callback, panic_is_due)) = waits.lock().unwrap().take() {
                in_the_callback.send(()).unwrap();
                let _ = panic_is_due.recv();
                panic!("the callback's panic");
            }
        }),
    );
    watch.heard();
    is_in_it.recv_timeout(WAIT).unwrap();
    watch.heard();
    drop(panic_now);
    eventually("the notice given in the panicking call", || {
        told.lock().unwrap().len() == 2
    });

    eventually("that thread's end", || watch.idle());
    watch.dial(&address, generation).unwrap().unwrap();
    watch.heard();
    eventually("a later notice", || told.lock().unwrap().len() == 3);
    watch.end();
}

/// A round of the retry thread that panics loses nothing owed: no later change is needed.
#[test]
fn a_panic_in_a_retry_round_loses_no_owed_notice() {
    let fixture = Fixture::new("panicking_retry", AllowAll);
    let raw = fixture.raw();
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    fixture.registry.panic_in_the_next_retry_round();
    fixture.registry.fail_sends(1);
    let generation = raw
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    eventually("the owed notice", || !told.0.lock().unwrap().is_empty());
    assert_eq!(*told.0.lock().unwrap(), [generation]);
    eventually("the thread's end", || !fixture.registry.retrying());
}

/// A notice during a round asks again, as that round's lookup may have gone before the change.
#[test]
fn a_notice_during_a_round_is_not_lost() {
    let fixture = Fixture::new("during_a_round", AllowAll);
    let raw = fixture.raw();
    let (_publisher, _serving, _address, _generation) = served(&raw, "a.name");
    let (entered, in_the_call) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let waits = Mutex::new(Some((entered, released)));
    let calls = Arc::new(AtomicU32::new(0));
    let counted = calls.clone();
    let resolver = fixture.resolver();
    let watch = resolver.reconciler(
        "a.name",
        Box::new(move |_| {
            counted.fetch_add(1, Ordering::SeqCst);
            if let Some((entered, released)) = waits.lock().unwrap().take() {
                entered.send(()).unwrap();
                let _ = released.recv();
            }
        }),
    );
    watch.heard();
    in_the_call.recv_timeout(WAIT).unwrap();
    watch.heard();
    drop(release);
    eventually("the round after", || calls.load(Ordering::SeqCst) == 2);

    // At each distance from the call before it, with no sleep: some meet the thread on its way out.
    for distance in 0..2_000u32 {
        let before = calls.load(Ordering::SeqCst);
        watch.heard();
        let deadline = Instant::now() + WAIT;
        while calls.load(Ordering::SeqCst) == before {
            assert!(Instant::now() < deadline, "a notice with no round");
            std::hint::spin_loop();
        }
        for _ in 0..distance * 20 {
            std::hint::spin_loop();
        }
    }
    watch.end();
}

/// A lookup reached this generation first: the notice still calls, with the lookup's binder.
#[test]
fn a_notice_of_a_generation_a_lookup_reached_still_calls() {
    let fixture = Fixture::new("repeat", AllowAll);
    let raw = fixture.raw();
    let (_publisher, _serving, address, generation) = served(&raw, "a.name");
    let told = Arc::new(Mutex::new(Vec::new()));
    let keep = told.clone();
    let resolver = fixture.resolver();
    let watch = resolver.reconciler(
        "a.name",
        Box::new(move |resolved: BinderResult<Resolved>| {
            keep.lock().unwrap().push(resolved.unwrap().into_binder());
        }),
    );
    let looked = watch.dial(&address, generation).unwrap().unwrap();
    watch.heard();
    eventually("the notice's call", || !told.lock().unwrap().is_empty());
    assert_eq!(&told.lock().unwrap()[0], looked.binder());
    watch.end();
}

/// A notice's connection and its call are one step for a lookup of a newer generation.
#[test]
fn a_lookup_waits_for_the_call_of_an_older_generation_and_no_older_call_follows_it() {
    let fixture = Fixture::new("order", AllowAll);
    let raw = fixture.raw();
    let (_first, _first_serving, _address, _first_generation) = served(&raw, "a.first");
    let (_second, _second_serving, second, second_generation) = served(&raw, "a.second");
    let (entered, running) = mpsc::channel();
    let (release, released) = mpsc::channel::<()>();
    let (entered, released) = (Mutex::new(entered), Mutex::new(released));
    let calls = Arc::new(Mutex::new(0));
    let counted = calls.clone();
    let resolver = fixture.resolver();
    let watch = resolver.reconciler(
        "a.first",
        Box::new(move |_| {
            *counted.lock().unwrap() += 1;
            entered.lock().unwrap().send(()).unwrap();
            let _ = released.lock().unwrap().recv();
        }),
    );
    watch.heard();
    running.recv_timeout(WAIT).unwrap();

    let (dialed, dial_returned) = mpsc::channel();
    let looking = watch.clone();
    let lookup = std::thread::spawn(move || {
        let reached = looking
            .dial(&second, second_generation)
            .map(|r| r.is_some());
        dialed.send(()).unwrap();
        reached
    });
    assert!(dial_returned
        .recv_timeout(Duration::from_millis(100))
        .is_err());
    drop(release);
    assert_eq!(lookup.join().unwrap().ok(), Some(true));

    watch.heard();
    eventually("the older notice's thread", || watch.idle());
    assert_eq!(*calls.lock().unwrap(), 1);
    watch.end();
}

/// Blocks in the subscription call until released, as a publisher that stops there.
struct StoppedTopic {
    entered: Mutex<mpsc::Sender<()>>,
    released: Mutex<mpsc::Receiver<()>>,
}

impl Interface for StoppedTopic {}

impl ITopic for StoppedTopic {
    fn r#subscribe(&self, _: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        self.entered.lock().unwrap().send(()).unwrap();
        let _ = self.released.lock().unwrap().recv();
        Err(StatusCode::DeadObject.into())
    }
}

/// `ResolverOptions::timeout` leaves this call without a deadline: the next generation ends it.
#[test]
fn a_publisher_stopped_in_the_subscription_call_holds_no_watch_from_the_next_one() {
    let fixture = Fixture::new("stopped_call", AllowAll);
    let (entered, running) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let stopped = BnTopic::new_binder(StoppedTopic {
        entered: Mutex::new(entered),
        released: Mutex::new(released),
    });
    let resolver = fixture.resolver();
    let _name = resolver.advertise("a.name", stopped.as_binder()).unwrap();
    let mut subscriber = named(&fixture, "a.name");
    running.recv_timeout(WAIT).unwrap();

    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    assert!(subscriber.wait_connected(WAIT));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    drop(release);
}

/// A [`UdsResolver`] that keeps a weak reference to what each binder's session is held by.
struct SessionsSeen {
    resolver: UdsResolver,
    seen: Arc<Mutex<Vec<Weak<dyn Any + Send + Sync>>>>,
}

impl Resolver for SessionsSeen {
    fn advertise(&self, name: &str, topic: rsbinder::SIBinder) -> BinderResult<Advertisement> {
        self.resolver.advertise(name, topic)
    }

    fn watch(
        &self,
        name: &str,
        on_registered: Box<dyn Fn(rsbinder::SIBinder) + Send + Sync>,
    ) -> BinderResult<Watch> {
        self.resolver.watch(name, on_registered)
    }

    fn watch_resolved(&self, name: &str, on_resolved: OnResolved) -> BinderResult<Watch> {
        let seen = self.seen.clone();
        let spied = Box::new(move |resolved: BinderResult<Resolved>| {
            if let Ok((_, session)) = resolved.clone().map(Resolved::into_parts) {
                seen.lock()
                    .unwrap()
                    .extend(session.as_ref().map(Arc::downgrade));
            }
            on_resolved(resolved);
        });
        self.resolver.watch_resolved(name, spied)
    }

    fn host(&self) -> Option<rsbinder::SIBinder> {
        self.resolver.host()
    }
}

/// Nothing else ends this call: the drop has to let go of all that holds the session open.
#[test]
fn a_dropped_subscriber_closes_the_session_of_a_publisher_stopped_in_the_call() {
    let fixture = Fixture::new("stopped_drop", AllowAll);
    let (entered, running) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let stopped = BnTopic::new_binder(StoppedTopic {
        entered: Mutex::new(entered),
        released: Mutex::new(released),
    });
    let resolver = fixture.resolver();
    let _name = resolver.advertise("a.name", stopped.as_binder()).unwrap();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let spy = SessionsSeen {
        resolver: fixture.resolver(),
        seen: seen.clone(),
    };
    let subscriber = NamedSubscriber::<Bytes>::subscribe_with("a.name", &policy(), &spy).unwrap();
    running.recv_timeout(WAIT).unwrap();
    assert!(!seen.lock().unwrap().is_empty());
    drop(subscriber);
    eventually("the session to close", || {
        seen.lock().unwrap().iter().all(|s| s.strong_count() == 0)
    });
    drop(release);
}

/// With no room for it, the host owes the notice: nothing else would tell this watcher.
#[test]
fn a_notice_with_no_room_is_sent_again() {
    let fixture = Fixture::new("no_room", AllowAll);
    let raw = fixture.raw();
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    fixture.registry.fail_sends(1);
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    let generation = raw
        .registry
        .r#commit("a.name", &topic, allocation.allocation)
        .unwrap();
    eventually("the notice", || !told.0.lock().unwrap().is_empty());
    assert_eq!(*told.0.lock().unwrap(), [generation]);
    eventually("nothing owed", || fixture.registry.stats().owed == 0);
    assert_eq!(fixture.registry.stats().notices_retried, 1);
}

/// Stops in its first notice, so the notices after it fill its socket.
struct StoppedWatcher(Mutex<mpsc::Receiver<()>>);

impl Interface for StoppedWatcher {}

impl ITopicAddressWatcher for StoppedWatcher {
    fn r#onAdvertised(&self, _: &str, _: i64) -> BinderResult<()> {
        let _ = self.0.lock().unwrap().recv();
        Ok(())
    }
}

/// A notice to a second address while the first is unpaid ends the session, and only that session.
#[test]
fn a_session_with_two_unpaid_addresses_ends_and_no_other_does() {
    let fixture = Fixture::new("unpaid", AllowAll);
    let raw = fixture.raw();
    let unpaying = fixture.raw();
    let (release, released) = mpsc::channel();
    let names = ["a.one", "a.two"];
    let stops = BnTopicAddressWatcher::new_binder(StoppedWatcher(Mutex::new(released)));
    unpaying.registry.r#watch(names[0], &stops).unwrap();
    let silent = BnTopicAddressWatcher::new_binder(Silent);
    unpaying.registry.r#watch(names[1], &silent).unwrap();
    let told = Counted::default();
    let listening = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch(names[1], &listening).unwrap();
    assert_eq!(fixture.registry.stats().watchers, 3);

    let (_publisher, topic) = topic();
    let mut last = 0;
    for name in names {
        let allocation = raw.registry.r#allocate(name, &topic).unwrap();
        last = raw
            .registry
            .r#commit(name, &topic, allocation.allocation)
            .unwrap();
    }
    within(WAIT, "the unpaying session's watches gone", || {
        fixture.registry.stats().watchers == 1
    });
    within(WAIT, "the other session's notice", || {
        told.0.lock().unwrap().last() == Some(&last)
    });
    assert!(!raw.session.0.is_ended());
    // Only now can it see the end: its one incoming thread was in the notice that stopped.
    drop(release);
    within(WAIT, "the unpaying session's end", || {
        unpaying.session.0.is_ended()
    });
}

/// A send deadline no test of a stopped session comes near, so that it ends none of them midway.
const NO_SEND_DEADLINE: Duration = Duration::from_secs(600);

/// A registry whose stopped sessions outlast the test, however slowly the machine runs it.
fn with_no_send_deadline(tag: &str) -> Fixture {
    Fixture::with_server(tag, AllowAll, |server| {
        server.set_reply_timeout(Some(NO_SEND_DEADLINE)).unwrap();
    })
}

/// Each step's 5 s is under the deadline a send would wait for, on the thread making the change.
#[test]
fn a_watcher_that_stopped_holds_up_no_commit_and_no_other_watcher() {
    /// Changes after the first notice with no room, so some are made with its socket full.
    const WITH_THE_SOCKET_FULL: usize = 50;
    let fixture = with_no_send_deadline("stopped_watcher");
    let raw = fixture.raw();
    let stopped = fixture.raw();
    let (release, released) = mpsc::channel();
    let watcher = BnTopicAddressWatcher::new_binder(StoppedWatcher(Mutex::new(released)));
    stopped.registry.r#watch("a.name", &watcher).unwrap();
    let other = fixture.raw();
    let told = Counted::default();
    let listening = BnTopicAddressWatcher::new_binder(told.clone());
    other.registry.r#watch("a.name", &listening).unwrap();

    let (_publisher, topic) = topic();
    let mut generations = Vec::new();
    let mut with_the_socket_full = 0;
    let filling = Instant::now();
    while with_the_socket_full < WITH_THE_SOCKET_FULL {
        assert!(
            filling.elapsed() < Duration::from_secs(120),
            "the stopped watcher's socket has room after {} notices",
            generations.len()
        );
        let started = Instant::now();
        let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
        let generation = raw
            .registry
            .r#commit("a.name", &topic, allocation.allocation)
            .unwrap();
        generations.push(generation);
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "a commit waited"
        );
        // One at a time, so the notice of each is seen before the next change.
        within(Duration::from_secs(5), "the other watcher's notice", || {
            told.0.lock().unwrap().last() == Some(&generation)
        });
        if fixture.registry.stats().owed > 0 {
            with_the_socket_full += 1;
        }
    }
    assert_eq!(*told.0.lock().unwrap(), generations);
    drop(release);
}

/// A stopped session's socket is full: a commit or an eviction drops its proxies with no wait.
#[test]
fn the_last_drops_of_a_stopped_session_hold_up_no_commit_and_no_eviction() {
    let limit = Duration::from_secs(5);
    let access = Arc::new(EvictsAfterOne(Mutex::new(None)));
    // A session of many addresses, which only the limit on them ends before it stops.
    let fixture = Fixture::with_server("stopped_drops", access.clone(), |server| {
        server.__limit_unpaid_addresses(usize::MAX);
    });
    let raw = fixture.raw();
    let stopped = fixture.raw();
    let (release, released) = mpsc::channel();
    let first = BnTopicAddressWatcher::new_binder(StoppedWatcher(Mutex::new(released)));
    stopped.registry.r#watch("a.name", &first).unwrap();
    let mut watches = vec![first];
    for _ in 1..MAX_WATCHERS {
        let watcher = BnTopicAddressWatcher::new_binder(Silent);
        stopped.registry.r#watch("a.name", &watcher).unwrap();
        watches.push(watcher);
    }
    let (_kept_alive, stopped_topic) = topic();
    let held = stopped
        .registry
        .r#allocate("a.held", &stopped_topic)
        .unwrap();
    stopped
        .registry
        .r#commit("a.held", &stopped_topic, held.allocation)
        .unwrap();

    let (_publisher, other_topic) = topic();
    let mut last = 0;
    let filling = Instant::now();
    loop {
        assert!(
            filling.elapsed() < Duration::from_secs(60),
            "no notice owed"
        );
        let allocation = raw.registry.r#allocate("a.name", &other_topic).unwrap();
        let started = Instant::now();
        raw.registry
            .r#commit("a.name", &other_topic, allocation.allocation)
            .unwrap();
        assert!(started.elapsed() < limit, "a commit waited");
        let sent = fixture.registry.stats().notices_sent;
        if fixture.registry.stats().owed > 0 && sent == last {
            break;
        }
        last = sent;
    }

    let allocation = raw.registry.r#allocate("a.held", &other_topic).unwrap();
    let started = Instant::now();
    raw.registry
        .r#commit("a.held", &other_topic, allocation.allocation)
        .unwrap();
    assert!(started.elapsed() < limit, "a commit waited");

    *access.0.lock().unwrap() = Some(1);
    let watcher = BnTopicAddressWatcher::new_binder(Silent);
    let started = Instant::now();
    raw.registry.r#watch("a.name", &watcher).unwrap();
    assert!(started.elapsed() < limit, "an eviction waited");
    assert_eq!(fixture.registry.stats().watchers, 1);
    drop(release);
}

/// Once told how many to allow, refuses `Subscribe` past them, as a reload does to a full name.
struct EvictsAfterOne(Mutex<Option<u32>>);

impl RegistryAccess for Arc<EvictsAfterOne> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        // The retry thread judges an owed notice whenever it is due: its turn takes no count.
        if std::thread::current().name() == Some("rsbinder-topic-registry") {
            return true;
        }
        let mut left = self.0.lock().unwrap();
        match left.as_mut() {
            Some(left) if action == Action::Subscribe => {
                let allowed = *left > 0;
                *left = left.saturating_sub(1);
                allowed
            }
            _ => true,
        }
    }
}

/// Stops in its first notice until released, then keeps what it hears.
struct WatcherThatGoesOn {
    released: Mutex<Option<mpsc::Receiver<()>>>,
    told: Counted,
}

impl Interface for WatcherThatGoesOn {}

impl ITopicAddressWatcher for WatcherThatGoesOn {
    fn r#onAdvertised(&self, name: &str, gen: i64) -> BinderResult<()> {
        if let Some(released) = self.released.lock().unwrap().take() {
            let _ = released.recv();
        }
        self.told.r#onAdvertised(name, gen)
    }
}

/// Past its socket's room a watcher hears nothing out of order, and the newest generation last.
#[test]
fn a_watcher_that_goes_on_hears_the_generations_in_order_up_to_the_newest() {
    let fixture = Fixture::new("goes_on", AllowAll);
    let raw = fixture.raw();
    let slow = fixture.raw();
    let (release, released) = mpsc::channel();
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(WatcherThatGoesOn {
        released: Mutex::new(Some(released)),
        told: told.clone(),
    });
    slow.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    let mut newest = 0;
    let commit = || {
        let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
        raw.registry
            .r#commit("a.name", &topic, allocation.allocation)
            .unwrap()
    };
    let filling = Instant::now();
    while fixture.registry.stats().owed == 0 {
        assert!(
            filling.elapsed() < Duration::from_secs(60),
            "no notice owed"
        );
        newest = commit();
    }
    for _ in 0..10 {
        newest = commit();
    }
    drop(release);
    eventually("the newest generation", || {
        told.0.lock().unwrap().last() == Some(&newest)
    });
    let heard = told.0.lock().unwrap().clone();
    assert!(heard.is_sorted(), "out of order: {heard:?}");
    eventually("nothing owed", || fixture.registry.stats().owed == 0);
    assert!(fixture.registry.stats().notices_retried > 0);
}

/// The descriptors of `dir`'s sockets this process holds: what a registry made and has not closed.
fn addresses_held(dir: &std::path::Path) -> usize {
    let open = std::fs::read_dir("/proc/self/fd").unwrap();
    let links = open.filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok());
    links
        .filter(|link| link.starts_with(dir) && link != dir)
        .count()
}

/// Notices past a stopped watcher's room leave the host one owed entry and no descriptor of theirs.
#[test]
fn a_stopped_watcher_costs_the_host_one_notice_owed_however_many_changes() {
    const CHANGES: usize = 400;
    let fixture = with_no_send_deadline("bounded");
    let dir = fixture.path.with_file_name("topic.d");
    let raw = fixture.raw();
    let stopped = fixture.raw();
    let (release, released) = mpsc::channel();
    let watcher = BnTopicAddressWatcher::new_binder(StoppedWatcher(Mutex::new(released)));
    stopped.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    let change = || {
        let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
        raw.registry
            .r#commit("a.name", &topic, allocation.allocation)
            .unwrap();
    };
    let filling = Instant::now();
    while fixture.registry.stats().owed == 0 {
        assert!(
            filling.elapsed() < Duration::from_secs(120),
            "no notice owed"
        );
        change();
    }
    for _ in 0..CHANGES {
        change();
    }
    let stats = fixture.registry.stats();
    assert_eq!(stats.owed, 1, "{stats:?}");
    // The name's, the test's copy of the last. A notice holds none.
    let held = addresses_held(&dir);
    assert!(held <= 2, "{held} addresses held after {CHANGES} changes");
    drop(release);
}

/// A notice's frame is one a send that does not wait takes, for the longest name there is.
#[test]
fn a_notice_of_the_longest_name_is_sent() {
    let fixture = Fixture::new("longest", AllowAll);
    let raw = fixture.raw();
    let name = "n".repeat(127);
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch(&name, &watcher).unwrap();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate(&name, &topic).unwrap();
    let generation = raw
        .registry
        .r#commit(&name, &topic, allocation.allocation)
        .unwrap();
    eventually("the notice", || !told.0.lock().unwrap().is_empty());
    assert_eq!(*told.0.lock().unwrap(), [generation]);
    assert_eq!(fixture.registry.stats().notices_failed, 0);
}

/// A client's end of a socket that keeps the frames it sends.
struct Recording(UnixTransport, Arc<Mutex<Vec<Vec<u8>>>>);

impl Recording {
    fn keep(&self, frame: &[u8]) {
        self.1.lock().unwrap().push(frame.to_vec());
    }
}

impl RpcTransport for Recording {
    fn send_raw(&self, buf: &[u8]) -> RpcResult<()> {
        self.keep(buf);
        self.0.send_raw(buf)
    }
    fn send_raw_with_fds(&self, buf: &[u8], fds: &[BorrowedFd<'_>]) -> RpcResult<()> {
        self.keep(buf);
        self.0.send_raw_with_fds(buf, fds)
    }
    fn __send_no_wait(&self, buf: &[u8], fds: &[BorrowedFd<'_>]) -> Option<RpcResult<bool>> {
        self.keep(buf);
        self.0.__send_no_wait(buf, fds)
    }
    fn recv_raw(&self, buf: &mut [u8]) -> RpcResult<usize> {
        self.0.recv_raw(buf)
    }
    fn recv_raw_with_fds(&self, buf: &mut [u8]) -> RpcResult<(usize, Vec<OwnedFd>)> {
        self.0.recv_raw_with_fds(buf)
    }
    fn peer_identity(&self) -> PeerIdentity {
        self.0.peer_identity()
    }
    fn describe(&self) -> &str {
        "recording"
    }
    fn supports_fd_passing(&self) -> bool {
        self.0.supports_fd_passing()
    }
    fn shutdown(&self) -> RpcResult<()> {
        self.0.shutdown()
    }
}

/// When a notice is written changes, not what: its frame is the stub's but for the oneway number.
#[test]
fn a_notice_is_the_frame_the_generated_stub_writes() {
    let root = scratch("same_frame");
    let path = root.join("watcher.sock");
    let server = RpcServer::setup_unix_server(&path).unwrap();
    let watcher = BnTopicAddressWatcher::new_binder(Silent);
    let _serving = uds::serve(&server, watcher.as_binder()).unwrap();
    let frames = Arc::new(Mutex::new(Vec::new()));
    let transport = Recording(UnixTransport::connect(&path).unwrap(), frames.clone());
    let session = RpcSession::connect_android13plus_fd(
        Box::new(transport),
        uds::WIRE,
        FileDescriptorTransportMode::Unix,
    )
    .unwrap();
    let proxy: Strong<dyn ITopicAddressWatcher> =
        session.get_root().unwrap().into_interface().unwrap();
    frames.lock().unwrap().clear();
    super::uds::try_notice(&proxy, "a.name", 7).unwrap();
    proxy.r#onAdvertised("a.name", 7).unwrap();
    session.close_session();

    // `RpcWireHeader` (16 bytes) then address (8), code (4), flags (4) and `asyncNumber` (8).
    const NUMBER: std::ops::Range<usize> = 32..40;
    let frames = frames.lock().unwrap();
    assert_eq!(frames.len(), 2);
    let (mut ours, mut stubs) = (frames[0].clone(), frames[1].clone());
    assert_eq!(ours[NUMBER], 0u64.to_le_bytes());
    assert_eq!(stubs[NUMBER], 1u64.to_le_bytes());
    ours[NUMBER].fill(0);
    stubs[NUMBER].fill(0);
    assert_eq!(ours, stubs);
    std::fs::remove_dir_all(&root).unwrap();
}

/// Holds the judgement of a change's notice, which a change sends under `fire_order` alone.
struct HeldJudgement {
    entered: Mutex<mpsc::Sender<()>>,
    released: Mutex<mpsc::Receiver<()>>,
    armed: std::sync::atomic::AtomicBool,
}

impl RegistryAccess for Arc<HeldJudgement> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        if action == Action::Subscribe && self.armed.swap(false, Ordering::SeqCst) {
            let _ = self.entered.lock().unwrap().send(());
            let _ = self.released.lock().unwrap().recv();
        }
        true
    }
}

/// A retry judged before a newer change sends what the name has at the send, not at the judgement.
#[test]
fn a_notice_sent_again_is_of_the_generation_the_name_has_by_then() {
    let (entered, running) = mpsc::channel();
    let (held, released) = mpsc::channel::<()>();
    let access = Arc::new(HeldJudgement {
        entered: Mutex::new(entered),
        released: Mutex::new(released),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let fixture = Fixture::new("by_then", access.clone());
    // After `fixture`: on a panic it drops first, so the judgement is let go before a join.
    let release = held;
    let raw = fixture.raw();
    let told = Counted::default();
    let watcher = BnTopicAddressWatcher::new_binder(told.clone());
    raw.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    let commit = || {
        let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
        raw.registry
            .r#commit("a.name", &topic, allocation.allocation)
            .unwrap()
    };
    fixture.registry.fail_sends(1);
    let older = commit();
    access.armed.store(true, Ordering::SeqCst);
    running.recv_timeout(WAIT).unwrap();
    fixture.registry.fail_sends(1);
    let newer = commit();
    assert!(newer > older);
    drop(release);
    eventually("the notice", || !told.0.lock().unwrap().is_empty());
    assert_eq!(*told.0.lock().unwrap(), [newer]);
    eventually("nothing owed", || fixture.registry.stats().owed == 0);
}

/// `inner` is not held across what a change does for its notices: a judgement or a send.
#[test]
fn a_change_sends_its_notices_with_the_registrys_state_free() {
    let (entered, running) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let access = Arc::new(HeldJudgement {
        entered: Mutex::new(entered),
        released: Mutex::new(released),
        armed: std::sync::atomic::AtomicBool::new(false),
    });
    let fixture = Fixture::new("state_free", access.clone());
    let raw = fixture.raw();
    let watcher = BnTopicAddressWatcher::new_binder(Silent);
    raw.registry.r#watch("a.name", &watcher).unwrap();
    let (_publisher, topic) = topic();
    let allocation = raw.registry.r#allocate("a.name", &topic).unwrap();
    access.armed.store(true, Ordering::SeqCst);
    let registry = raw.registry.clone();
    let committing = std::thread::spawn(move || {
        registry
            .r#commit("a.name", &topic, allocation.allocation)
            .map(drop)
    });
    running.recv_timeout(WAIT).unwrap();
    let registry = fixture.registry.shared_for_a_test();
    let (asked, answered) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = asked.send(registry.stats());
    });
    let answer = answered.recv_timeout(Duration::from_secs(5));
    drop(release);
    if answer.is_err() {
        // Its `Drop` would join the worker that this regression leaves stuck on the state's lock.
        std::mem::forget(fixture);
        panic!("a judgement held the registry's state");
    }
    committing.join().unwrap().unwrap();
}

/// Holds a `list` call at the host, as a host that stopped holds any call on a session.
struct HeldList {
    entered: Mutex<mpsc::Sender<()>>,
    released: Mutex<mpsc::Receiver<()>>,
}

impl RegistryAccess for Arc<HeldList> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        if action == Action::List {
            let _ = self.entered.lock().unwrap().send(());
            let _ = self.released.lock().unwrap().recv();
        }
        true
    }
}

/// The session's one connection is taken: a drop that waited for it would hold the end record.
#[test]
fn a_host_that_holds_a_call_holds_no_publisher_drop_and_no_watch_drop() {
    let (entered, running) = mpsc::channel();
    let (release, released) = mpsc::channel();
    let access = Arc::new(HeldList {
        entered: Mutex::new(entered),
        released: Mutex::new(released),
    });
    let fixture = Fixture::new("held_host", access);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let watching =
        NamedSubscriber::<Bytes>::subscribe_with("b.name", &policy(), &resolver).unwrap();
    let mut subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));

    let listing = resolver.clone();
    let list = std::thread::spawn(move || listing.list("").map(|listed| listed.len()));
    running.recv_timeout(WAIT).unwrap();

    let (dropped, drops_returned) = mpsc::channel();
    std::thread::spawn(move || {
        drop(publisher);
        drop(watching);
        dropped.send(()).unwrap();
    });
    let returned = drops_returned.recv_timeout(WAIT / 2);
    let ended = subscriber.recv_timeout(Duration::from_millis(100));
    drop(release);
    assert!(returned.is_ok(), "a drop waited for the host");
    assert_eq!(
        ended,
        Err(RecvError::Disconnected(Disconnected::Ended(Closed::End)))
    );
    list.join().unwrap().map_err(|_: Status| ()).unwrap();
    eventually("the name to go", || fixture.registry.stats().topics == 0);
    eventually("the watch to go", || fixture.registry.stats().watched <= 1);
}

/// A session left to drain keeps the server's listener, on which a connect would go unanswered.
#[test]
fn a_dropped_publisher_refuses_a_connect_while_its_sessions_last() {
    let fixture = Fixture::new("refuses", AllowAll);
    let resolver = fixture.resolver();
    let publisher =
        Publisher::<Bytes>::advertise_with("a.name", &PublisherPolicy::default(), &resolver)
            .unwrap();
    let raw = fixture.raw();
    let address = OwnedFd::from(raw.registry.r#lookup("a.name").unwrap().socket.unwrap());
    let subscriber = named(&fixture, "a.name");
    assert!(subscriber.wait_connected(WAIT));
    drop(publisher);
    let refused = connect_through(&address).unwrap_err();
    assert_eq!(refused.raw_os_error(), Some(libc::ECONNREFUSED));
}

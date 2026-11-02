// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry over a Unix socket within one process: an RPC proxy is one in the same process too,
//! so a real registry, its publishers and its subscribers run with no binder device. The calls
//! that a resolver never makes wrongly are made by hand on the session.

use std::collections::BTreeSet;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::rpc::{RpcServer, RpcServerHandle};
use rsbinder::{Caller, ExceptionCode, Interface, ParcelFileDescriptor, StatusCode, Strong};

use super::uds::{ResolverOptions, SocketDir, UdsRegistry, UdsResolver};
use super::{Action, AllowAll, RegistryAccess};
use crate::aidl::ITopic::ITopic;
use crate::aidl::ITopicAddressWatcher::{BnTopicAddressWatcher, ITopicAddressWatcher};
use crate::aidl::ITopicUdsRegistry::ITopicUdsRegistry;
use crate::uds::{self, Endpoint, Options, SessionGuard};
use crate::{
    Bytes, Closed, Disconnected, NamedState, NamedSubscriber, Publisher, PublisherPolicy,
    RecvError, Resolver, SubscriberPolicy,
};

const WAIT: Duration = Duration::from_secs(10);

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
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
        let root = scratch(tag);
        let dir = SocketDir::open(&root.join("topic.d")).unwrap();
        let registry = UdsRegistry::new(dir, access);
        let path = root.join("topic.sock");
        let server = RpcServer::setup_unix_server(&path).unwrap();
        registry.serve(&server).unwrap();
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
    std::fs::write(path.join("left"), b"").unwrap();
    let _dir = SocketDir::open(&path).unwrap();
    assert_eq!(std::fs::read_dir(&path).unwrap().count(), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[test]
fn a_directory_others_can_write_or_a_symlink_is_not_used() {
    use std::os::unix::fs::PermissionsExt;
    let root = scratch("dir_bad");
    let open = root.join("open.d");
    std::fs::create_dir(&open).unwrap();
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o770)).unwrap();
    let refused = SocketDir::open(&open).unwrap_err();
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
    std::fs::set_permissions(&open, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = root.join("link.d");
    std::os::unix::fs::symlink(&open, &link).unwrap();
    assert!(SocketDir::open(&link).is_err());
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

/// Two publishers of a uid: the later commit wins, whichever allocated first, with a newer
/// generation.
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

/// Refuses every commit but the first, as a reload can, with the right to allocate kept.
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
struct WatchersGoAway(Mutex<bool>);

impl RegistryAccess for Arc<WatchersGoAway> {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        action != Action::Subscribe || !*self.0.lock().unwrap()
    }
}

/// Counts the notices of the watch.
#[derive(Default, Clone)]
struct Counted(Arc<Mutex<Vec<i64>>>);

impl Interface for Counted {}

impl ITopicAddressWatcher for Counted {
    fn r#onAdvertised(
        &self,
        _: &str,
        _: &ParcelFileDescriptor,
        generation: i64,
    ) -> rsbinder::BinderResult<()> {
        self.0.lock().unwrap().push(generation);
        Ok(())
    }
}

struct Silent;

impl Interface for Silent {}

impl ITopicAddressWatcher for Silent {
    fn r#onAdvertised(
        &self,
        _: &str,
        _: &ParcelFileDescriptor,
        _: i64,
    ) -> rsbinder::BinderResult<()> {
        Ok(())
    }
}

#[test]
fn a_watcher_that_lost_the_right_is_sent_no_descriptor() {
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
    std::thread::sleep(Duration::from_millis(50));
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

/// A commit the resolver loses keeps what the publisher had, and stops the server it made.
#[test]
fn a_commit_a_policy_refuses_leaves_the_publisher_its_name() {
    let access = Arc::new(ReloadedAway::default());
    let fixture = Fixture::new("keep_name", access.clone());
    let resolver = fixture.resolver();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    publisher.advertise_on("a.name", &resolver).unwrap();
    let raw = fixture.raw();
    let held = raw.registry.r#lookup("a.name").unwrap();
    *access.0.lock().unwrap() = true;
    let refused = publisher.advertise_on("b.name", &resolver).err().unwrap();
    assert_eq!(code(refused), ExceptionCode::Security);
    let found = raw.registry.r#lookup("a.name").unwrap();
    assert_eq!(found.generation, held.generation);
    assert!(found.socket.is_some());
    assert_eq!(fixture.registry.stats().pending, 0);
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
    assert_eq!(
        subscriber.recv_timeout(Duration::from_millis(50)),
        Err(RecvError::Timeout)
    );
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

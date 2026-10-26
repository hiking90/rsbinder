// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry of topic names in a process of its own (plan 15 AC-15.5.1b, 5.4–5.6): publishers and
//! subscribers reach it through `RegistryResolver`. `#[ignore]`d: they need a binder device and a
//! service manager that lets any name register, and has no registry of its own:
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p rsbinder-topic --test registry -- --ignored --test-threads=1
//! ```
//!
//! The host is a `child` with one binder thread, as `rsb_hub` has, registered under a name of the
//! test's own. A fake registry, also a `child`, answers as the tests tell it, to send a watch the
//! notices a registry would not.
//!
//! A stopped peer holds up a call only through what the call reaches. Receiving a topic's binder
//! asks its publisher nothing (plan 16), so a lookup of a stopped publisher's name waits on nothing
//! of that publisher, even as the first proxy of it in this process. An `onAdvertised` takes of a
//! watcher's async space what the kernel counts: its data and the offset of its one object, each
//! rounded up to 8 bytes. A resumed watcher meets the last publisher by a notice sent again or by
//! a lookup (plan 15 D14-e).

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use common::{eventually, init, name, say, wait_for_eof, Peer, Stopped, NAME, ROLE, WAIT};
use rsbinder::{
    hub, BinderResult, ExceptionCode, Interface, Parcel, ProcessState, SIBinder, Status,
    StatusCode, Strong, FIRST_CALL_TRANSACTION, FLAG_CLEAR_BUF, FLAG_PRIVATE_LOCAL,
};
use rsbinder_topic::aidl::ITopic::{BnTopic, ITopic, ERROR_CLOSED};
use rsbinder_topic::aidl::ITopicRegistry::{BnTopicRegistry, ITopicRegistry, SERVICE_NAME};
use rsbinder_topic::aidl::ITopicWatcher::{BnTopicWatcher, ITopicWatcher};
use rsbinder_topic::aidl::SubscribeRequest::SubscribeRequest;
use rsbinder_topic::aidl::TopicEndpoint::TopicEndpoint;
use rsbinder_topic::aidl::TopicInfo::TopicInfo;
use rsbinder_topic::aidl::TopicRef::TopicRef;
use rsbinder_topic::registry::{AllowAll, Registry, RegistryResolver};
use rsbinder_topic::{
    Bytes, Closed, Disconnected, NamedSubscriber, Publisher, PublisherPolicy, RecvError, Resolver,
    Start, SubscriberPolicy,
};

/// Where a child finds the registry.
const REGISTRY: &str = "RSBINDER_TOPIC_REGISTRY";

/// The service name of a test's registry, apart from the hub's own.
fn registry_name(tag: &str) -> String {
    format!("{SERVICE_NAME}.test.{tag}.{}", std::process::id())
}

/// A registry host, and a resolver of it.
fn host(registry: &str) -> (Peer, RegistryResolver) {
    init();
    let peer = Peer::spawn("host", registry);
    assert_eq!(peer.says(), "ready");
    (peer, resolver(registry))
}

fn resolver(registry: &str) -> RegistryResolver {
    RegistryResolver::new(hub::check_service(registry).unwrap()).unwrap()
}

/// The registry's own interface, for the calls no resolver makes.
fn client(registry: &str) -> Strong<dyn ITopicRegistry> {
    hub::check_service(registry)
        .unwrap()
        .into_interface()
        .unwrap()
}

/// The host registry's failed notices and the watches it owes one.
fn stats(host: &mut Peer) -> (u64, usize) {
    host.tell("stats");
    let said = host.says();
    match said.split_whitespace().collect::<Vec<_>>().as_slice() {
        ["stats", failed, owed] => (failed.parse().unwrap(), owed.parse().unwrap()),
        _ => panic!("no stats: {said}"),
    }
}

/// `call`, which ends within a second; one held up by a `stopped` peer fails once they resume.
fn timed<T>(what: &str, stopped: &[Stopped], call: impl FnOnce() -> T) -> T {
    let (ended, end) = mpsc::channel::<()>();
    let began = Instant::now();
    std::thread::scope(|scope| {
        // Not the caller: a thread that made a binder call pays `BINDER_THREAD_EXIT` as it ends.
        let watchdog = scope.spawn(move || {
            let late = matches!(
                end.recv_timeout(Duration::from_secs(1)),
                Err(mpsc::RecvTimeoutError::Timeout)
            );
            if late {
                stopped.iter().for_each(Stopped::resume);
            }
            late
        });
        let done = call();
        drop(ended);
        assert!(
            !watchdog.join().unwrap(),
            "{what} took {:?}",
            began.elapsed()
        );
        done
    })
}

fn listed(resolver: &RegistryResolver, name: &str) -> Option<TopicInfo> {
    let mut infos = resolver.list(name).unwrap();
    infos.retain(|info| info.name == name);
    infos.pop()
}

/// A child that advertises `name` through `registry` and publishes as told.
fn publisher(name: &str, registry: &str) -> Peer {
    let peer = Peer::spawn_with("advertise", name, &[(REGISTRY, registry)]);
    assert_eq!(peer.says(), "ready");
    peer
}

fn published(peer: &mut Peer, count: u8) {
    peer.tell(&format!("publish {count}"));
    assert_eq!(peer.says(), "published");
}

/// AC-15.5.6: a subscriber started first meets the publisher, then the one that replaces it.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_subscriber_started_first_meets_the_publisher_and_the_one_started_after_it() {
    let registry = registry_name("first");
    let (_host, resolver) = host(&registry);
    let name = name("first");
    let policy = SubscriberPolicy::default();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &resolver).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(100)));
    let mut peer = publisher(&name, &registry);
    assert!(subscriber.wait_connected(WAIT));
    published(&mut peer, 2);
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![0])));
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    peer.kill();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::PublisherDied
        )))
    );
    let mut peer = publisher(&name, &registry);
    published(&mut peer, 3);
    for byte in 0..3 {
        assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![byte])));
    }
}

/// AC-15.5.6: no other oneway queued at the host, a closed or dropped name is gone at once.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_closed_or_dropped_publishers_name_is_gone_from_the_next_call() {
    let registry = registry_name("close");
    let (_host, resolver) = host(&registry);
    let name = name("close");
    let policy = PublisherPolicy::default();
    let publisher = Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap();
    assert!(listed(&resolver, &name).is_some());
    publisher.close(&Status::from(ExceptionCode::None));
    assert!(listed(&resolver, &name).is_none());
    let publisher = Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap();
    assert!(listed(&resolver, &name).is_some());
    drop(publisher);
    assert!(listed(&resolver, &name).is_none());
}

/// AC-15.5.6: a hub with no registry has none to find; this one has no feature `topic`.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_hub_with_no_registry_has_none_to_find() {
    init();
    let missing = RegistryResolver::find().unwrap_err();
    assert_eq!(missing.transaction_error(), StatusCode::NameNotFound);
}

/// AC-15.5.4: a dead publisher's name and a dead watcher's watch go.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_dead_publishers_name_and_a_dead_watchers_watch_go() {
    let registry = registry_name("death");
    let (_host, resolver) = host(&registry);
    let name = name("death");
    let mut publisher = publisher(&name, &registry);
    let watcher = Peer::spawn_with("subscribe", &name, &[(REGISTRY, &registry)]);
    assert_eq!(watcher.says(), "watching");
    assert_eq!(listed(&resolver, &name).map(|info| info.watchers), Some(1));
    drop(watcher);
    eventually("the dead watcher's watch to go", || {
        listed(&resolver, &name).map(|info| info.watchers) == Some(0)
    });
    publisher.kill();
    eventually("the dead publisher's name to go", || {
        listed(&resolver, &name).is_none()
    });
}

/// What a watch passed on, in order.
fn recorded() -> (
    Box<dyn Fn(SIBinder) + Send + Sync>,
    mpsc::Receiver<SIBinder>,
) {
    let (send, heard) = mpsc::channel();
    let send = Mutex::new(send);
    let record = move |binder| {
        let _ = send.lock().unwrap().send(binder);
    };
    (Box::new(record), heard)
}

/// AC-15.5.6, D13-f, D14-c: older notices are dropped; failed and dropped watches are ended.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_watch_drops_older_notices_and_ends_what_it_began() {
    init();
    let registry = registry_name("fake");
    let mut fake = Peer::spawn("fake", &registry);
    assert_eq!(fake.says(), "ready");
    let resolver = resolver(&registry);
    let name = name("fake");
    let policy = PublisherPolicy::default();
    let first = Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap();
    assert_eq!(fake.says(), "advertised 0");
    let second = Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap();
    assert_eq!(fake.says(), "advertised 1");

    let (record, heard) = recorded();
    let watch = resolver.watch(&name, record).unwrap();
    assert_eq!(fake.says(), "watched");
    assert!(
        heard.try_recv().is_ok_and(|b| b == second.as_binder()),
        "the reply's topic"
    );
    // The reply was generation 4: the first notice is older, the second newer.
    fake.tell("notify 0 3");
    assert_eq!(fake.says(), "notified");
    fake.tell("notify 1 6");
    assert_eq!(fake.says(), "notified");
    let next = heard.recv_timeout(WAIT).unwrap();
    assert!(next == second.as_binder(), "the older notice passed on");
    drop(watch);
    assert_eq!(fake.says(), "unwatched");

    fake.tell("refuse-watch");
    assert_eq!(fake.says(), "ok");
    let (record, _heard) = recorded();
    let refused = resolver.watch(&name, record).unwrap_err();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
    assert_eq!(fake.says(), "refused");
    assert_eq!(fake.says(), "unwatched");

    drop(first);
    assert_eq!(fake.says(), "withdrawn 0");
    drop(second);
    assert_eq!(fake.says(), "withdrawn 1");
}

/// A failed advertise withdraws the name the registry may keep, like a failed watch (D13-f).
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_failed_advertise_withdraws_its_name() {
    init();
    let registry = registry_name("refused");
    let mut fake = Peer::spawn("fake", &registry);
    assert_eq!(fake.says(), "ready");
    let resolver = resolver(&registry);
    let name = name("refused");
    fake.tell("refuse-advertise");
    assert_eq!(fake.says(), "ok");
    let policy = PublisherPolicy::default();
    let refused = Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap_err();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
    assert_eq!(fake.says(), "advertised 0");
    assert_eq!(fake.says(), "withdrawn 0");
}

/// AC-15.5.6, plan 15 D8: connections outlive the host; a new one knows only new publishers.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_new_host_knows_only_the_publishers_made_again() {
    let registry = registry_name("restart");
    let (mut first_host, resolver) = host(&registry);
    let name = name("restart");
    let policy = SubscriberPolicy::default();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &resolver).unwrap();
    let mut peer = publisher(&name, &registry);
    assert!(subscriber.wait_connected(WAIT));
    first_host.kill();
    let gone = resolver.list("").unwrap_err();
    assert_eq!(gone.transaction_error(), StatusCode::DeadObject);
    published(&mut peer, 1);
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![0])));

    let second_host = Peer::spawn("host", &registry);
    assert_eq!(second_host.says(), "ready");
    let resolver = self::resolver(&registry);
    assert!(listed(&resolver, &name).is_none());
    let mut again = NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &resolver).unwrap();
    assert!(!again.wait_connected(Duration::from_millis(100)));
    let mut made_again = publisher(&name, &registry);
    assert!(again.wait_connected(WAIT));
    published(&mut made_again, 1);
    assert_eq!(again.recv_timeout(WAIT), Ok(Bytes(vec![0])));
}

/// AC-15.5.1b: another interface's binder is taken, and the name's subscribers get `BadType`.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_binder_of_another_interface_is_taken_and_refused_to_subscribers() {
    let registry = registry_name("type");
    let (_host, resolver) = host(&registry);
    let name = name("type");
    let registry = hub::check_service(&registry).unwrap();
    let proxy = registry.as_proxy().unwrap();
    let mut data = proxy.prepare_transact(true).unwrap();
    data.write(name.as_str()).unwrap();
    data.write(&BnTopicWatcher::new_binder(Silent).as_binder())
        .unwrap();
    // `advertise`, the interface's first method.
    let flags = FLAG_CLEAR_BUF | FLAG_PRIVATE_LOCAL;
    proxy
        .submit_transact(FIRST_CALL_TRANSACTION, &data, flags)
        .unwrap();
    assert!(listed(&resolver, &name).is_some());

    // Our own binder: the resolver's `watch` reply fails to read it as `ITopic`.
    let policy = SubscriberPolicy::default();
    let refused = NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &resolver)
        .map(|_| ())
        .unwrap_err();
    assert_eq!(refused.transaction_error(), StatusCode::BadType);
}

/// AC-15.5.1b: a stopped publisher holds up no call; this process's first lookup of it ends.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_stopped_publisher_holds_up_no_call_of_the_registry() {
    let registry = registry_name("stop");
    let (_host, resolver) = host(&registry);
    let client = client(&registry);
    let stopped_name = name("stop.a");
    let other = name("stop.b");
    let stopping = publisher(&stopped_name, &registry);
    let stopped = [stopping.stop()];

    let found = timed("a lookup of the stopped publisher's name", &stopped, || {
        client.r#lookup(&stopped_name).unwrap()
    });
    let found = found.topic.unwrap();
    let again = client.r#lookup(&stopped_name).unwrap().topic.unwrap();
    assert!(found.as_binder() == again.as_binder());
    let names = timed("a list", &stopped, || resolver.list("").unwrap());
    assert!(names.iter().any(|info| info.name == stopped_name));
    let policy = PublisherPolicy::default();
    let _other = timed("an advertise of another name", &stopped, || {
        Publisher::<Bytes>::advertise_with(&other, &policy, &resolver).unwrap()
    });
    let looking = Peer::spawn_with("lookup", &other, &[(REGISTRY, &registry)]);
    let said = looking.says();
    let took: u64 = said.strip_prefix("found in ").unwrap().parse().unwrap();
    assert!(took < 1000, "another client's lookup took {took} ms");
}

/// AC-15.5.5 (D14-e): stopped watchers hold up no call; resumed, each meets the last publisher.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn stopped_watchers_hold_up_no_call_and_each_meets_the_last_publisher_once_resumed() {
    const WATCHERS: usize = 8;
    let registry = registry_name("watchers");
    let (mut host, resolver) = host(&registry);
    let client = client(&registry);
    let name = name("watchers");
    let env = [(REGISTRY, registry.as_str())];
    let watchers: Vec<Peer> = (0..WATCHERS)
        .map(|_| Peer::spawn_with("last", &name, &env))
        .collect();
    for watcher in &watchers {
        assert_eq!(watcher.says(), "watching");
    }
    // A watcher's async space is half of its mapping, the default one; a quarter more than fits.
    let size = notice_size(&client, &name);
    let advertises = ProcessState::default_mmap_size() / 2 / size * 5 / 4;
    eprintln!("an onAdvertised of {name:?} takes {size} bytes: {advertises} advertises");
    let stopped: Vec<Stopped> = watchers.iter().map(Peer::stop).collect();

    // Ended topics, each a new binder, refuse their subscribers: no ring for each of them.
    let ended: Vec<Strong<dyn ITopic>> = (0..advertises)
        .map(|_| {
            let topic = BnTopic::new_binder(Ended);
            timed("an advertise", &stopped, || {
                client.r#advertise(&name, &topic).unwrap()
            });
            topic
        })
        .collect();
    let policy = PublisherPolicy::default();
    let last = timed("the last advertise", &stopped, || {
        Publisher::<Bytes>::advertise_with(&name, &policy, &resolver).unwrap()
    });
    let (failed, owed) = stats(&mut host);
    assert!(failed >= 1, "no notice failed");
    assert_eq!(owed, WATCHERS);
    let found = timed("a lookup", &stopped, || client.r#lookup(&name).unwrap());
    assert!(found
        .topic
        .is_some_and(|topic| topic.as_binder() == last.as_binder()));
    let looking = Peer::spawn_with("lookup", &name, &env);
    let took: u64 = looking
        .says()
        .strip_prefix("found in ")
        .unwrap()
        .parse()
        .unwrap();
    assert!(took < 1000, "another client's lookup took {took} ms");

    drop(stopped);
    let deadline = Instant::now() + Duration::from_secs(10);
    let done = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Published until each watcher has a record, or the deadline passes.
        scope.spawn(|| {
            while !done.load(Ordering::Relaxed) && Instant::now() < deadline {
                last.publish(&Bytes(vec![7])).unwrap();
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let heard: Vec<String> = watchers.iter().map(|w| w.says_by(deadline)).collect();
        done.store(true, Ordering::Relaxed);
        assert_eq!(heard, vec!["got 7"; WATCHERS]);
    });
    drop(ended);
}

/// One `onAdvertised` of `name` in a watcher's async space: data and one offset, 8-aligned.
fn notice_size(registry: &Strong<dyn ITopicRegistry>, name: &str) -> usize {
    let size = |write: &dyn Fn(&mut Parcel) -> rsbinder::Result<()>| {
        let mut parcel = Parcel::new();
        write(&mut parcel).unwrap();
        parcel.data_size()
    };
    let binder = registry.as_binder();
    let proxy = binder.as_proxy().unwrap();
    // The interface token the proxy writes, with the watcher's descriptor for the registry's.
    let header = proxy.prepare_transact(true).unwrap().data_size()
        - size(&|parcel| parcel.write(proxy.descriptor()));
    let token = header + size(&|parcel| parcel.write(<Silent as ITopicWatcher>::descriptor()));
    let topic = BnTopic::new_binder(Ended);
    let data = token
        + size(&|parcel| parcel.write(name))
        + size(&|parcel| parcel.write(&topic))
        + size(&|parcel| parcel.write(&0i64));
    data.next_multiple_of(8) + 8
}

/// The other process's side of the tests above.
#[test]
#[ignore = "run by the tests above as their other process"]
fn child() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    let name = std::env::var(NAME).unwrap();
    let registry = || resolver(&std::env::var(REGISTRY).unwrap());
    match role.to_str().unwrap() {
        "host" => {
            // One binder thread, this one, as `rsb_hub` runs; the device `init_default` opens.
            let device = if std::path::Path::new(rsbinder::DEFAULT_BINDER_PATH).exists() {
                rsbinder::DEFAULT_BINDER_PATH
            } else {
                rsbinder::LEGACY_BINDER_PATH
            };
            ProcessState::init(device, 0).unwrap();
            let host = Registry::new(AllowAll);
            hub::add_service(&name, host.as_binder()).unwrap();
            std::thread::spawn(move || {
                for line in std::io::stdin().lines().map_while(Result::ok) {
                    assert_eq!(line, "stats");
                    let stats = host.stats();
                    say(&format!("stats {} {}", stats.notices_failed, stats.owed));
                }
                std::process::exit(0);
            });
            say("ready");
            ProcessState::join_thread_pool().unwrap();
        }
        "fake" => {
            init();
            let fake = Fake::default();
            let state = fake.0.clone();
            let binder = BnTopicRegistry::new_binder(fake);
            hub::add_service(&name, binder.as_binder()).unwrap();
            say("ready");
            for line in std::io::stdin().lines().map_while(Result::ok) {
                fake_command(&state, &line);
            }
        }
        "advertise" => {
            init();
            let policy = PublisherPolicy::default();
            let publisher =
                Publisher::<Bytes>::advertise_with(&name, &policy, &registry()).unwrap();
            say("ready");
            // `publish N` publishes the bytes 0 to N - 1, until EOF.
            for line in std::io::stdin().lines().map_while(Result::ok) {
                let count: u8 = line
                    .trim()
                    .strip_prefix("publish ")
                    .unwrap()
                    .parse()
                    .unwrap();
                for byte in 0..count {
                    publisher.publish(&Bytes(vec![byte])).unwrap();
                }
                say("published");
            }
        }
        "subscribe" => {
            init();
            let policy = SubscriberPolicy::default();
            let _subscriber =
                NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &registry()).unwrap();
            say("watching");
            wait_for_eof();
        }
        "last" => {
            init();
            let policy = SubscriberPolicy {
                start: Start::Last,
                ..SubscriberPolicy::default()
            };
            let mut subscriber =
                NamedSubscriber::<Bytes>::subscribe_with(&name, &policy, &registry()).unwrap();
            say("watching");
            let got = loop {
                match subscriber.recv_timeout(6 * WAIT) {
                    Err(RecvError::Disconnected(_)) => continue,
                    got => break got.unwrap(),
                }
            };
            say(&format!("got {}", got.0[0]));
            wait_for_eof();
        }
        "lookup" => {
            init();
            let client = client(&std::env::var(REGISTRY).unwrap());
            let began = Instant::now();
            assert!(client.r#lookup(&name).unwrap().topic.is_some());
            say(&format!("found in {}", began.elapsed().as_millis()));
            wait_for_eof();
        }
        other => panic!("no role {other}"),
    }
}

/// A topic whose publisher ended: it refuses every subscriber.
struct Ended;

impl Interface for Ended {}

impl ITopic for Ended {
    fn r#subscribe(&self, _request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        Err(Status::new_service_specific_error(ERROR_CLOSED, None))
    }
}

/// A watcher told of nothing, sent where a topic belongs.
struct Silent;

impl Interface for Silent {}

impl ITopicWatcher for Silent {
    fn r#onAdvertised(&self, _: &str, _: &Strong<dyn ITopic>, _: i64) -> BinderResult<()> {
        Ok(())
    }
}

/// A registry that answers as the test tells it.
#[derive(Default)]
struct Fake(Arc<Mutex<FakeState>>);

#[derive(Default)]
struct FakeState {
    topics: Vec<Strong<dyn ITopic>>,
    watch: Option<(String, Strong<dyn ITopicWatcher>)>,
    refuse_watch: bool,
    refuse_advertise: bool,
}

/// The generation of the fake's `watch` reply.
const REPLY_GENERATION: i64 = 4;

/// `notify I G` sends the watch topic I with generation G; `refuse-*` fails the next such call.
fn fake_command(state: &Mutex<FakeState>, line: &str) {
    let mut state = state.lock().unwrap();
    let words: Vec<&str> = line.split_whitespace().collect();
    match words.as_slice() {
        ["notify", index, generation] => {
            let topic = &state.topics[index.parse::<usize>().unwrap()];
            let (name, watcher) = state.watch.as_ref().unwrap();
            watcher
                .r#onAdvertised(name, topic, generation.parse().unwrap())
                .unwrap();
            say("notified");
        }
        ["refuse-watch"] => {
            state.refuse_watch = true;
            say("ok");
        }
        ["refuse-advertise"] => {
            state.refuse_advertise = true;
            say("ok");
        }
        other => panic!("no command {other:?}"),
    }
}

impl Interface for Fake {}

impl ITopicRegistry for Fake {
    fn r#advertise(&self, _name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<()> {
        let mut state = self.0.lock().unwrap();
        state.topics.push(topic.clone());
        say(&format!("advertised {}", state.topics.len() - 1));
        // The name is kept, as when only the reply failed.
        if std::mem::take(&mut state.refuse_advertise) {
            return Err(Status::from(ExceptionCode::Security));
        }
        Ok(())
    }

    fn r#withdraw(&self, _name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<()> {
        let state = self.0.lock().unwrap();
        let index = state
            .topics
            .iter()
            .position(|held| held.as_binder() == topic.as_binder())
            .unwrap();
        say(&format!("withdrawn {index}"));
        Ok(())
    }

    fn r#lookup(&self, _name: &str) -> BinderResult<TopicRef> {
        Ok(TopicRef::default())
    }

    fn r#watch(&self, name: &str, watcher: &Strong<dyn ITopicWatcher>) -> BinderResult<TopicRef> {
        let mut state = self.0.lock().unwrap();
        if std::mem::take(&mut state.refuse_watch) {
            say("refused");
            return Err(Status::from(ExceptionCode::Security));
        }
        state.watch = Some((name.to_owned(), watcher.clone()));
        say("watched");
        Ok(TopicRef {
            topic: state.topics.last().cloned(),
            generation: REPLY_GENERATION,
        })
    }

    fn r#unwatch(&self, _name: &str, _watcher: &Strong<dyn ITopicWatcher>) -> BinderResult<()> {
        self.0.lock().unwrap().watch = None;
        say("unwatched");
        Ok(())
    }

    fn r#list(&self, _prefix: &str) -> BinderResult<Vec<TopicInfo>> {
        Ok(Vec::new())
    }
}

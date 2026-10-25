// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A registry of topic names in a process of its own (plan 15 AC-15.5.4, 5.6): publishers and
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

#![cfg(any(target_os = "linux", target_os = "android"))]

mod common;

use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use common::{eventually, init, name, say, wait_for_eof, Peer, NAME, ROLE, WAIT};
use rsbinder::{
    hub, BinderResult, ExceptionCode, Interface, ProcessState, SIBinder, Status, StatusCode, Strong,
};
use rsbinder_topic::aidl::ITopic::ITopic;
use rsbinder_topic::aidl::ITopicRegistry::{BnTopicRegistry, ITopicRegistry, SERVICE_NAME};
use rsbinder_topic::aidl::ITopicWatcher::ITopicWatcher;
use rsbinder_topic::aidl::TopicInfo::TopicInfo;
use rsbinder_topic::aidl::TopicRef::TopicRef;
use rsbinder_topic::registry::{AllowAll, Registry, RegistryResolver};
use rsbinder_topic::{
    Bytes, Closed, Disconnected, NamedSubscriber, Publisher, PublisherPolicy, RecvError, Resolver,
    SubscriberPolicy,
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
            // One binder thread, this one, as `rsb_hub` runs.
            ProcessState::init(rsbinder::DEFAULT_BINDER_PATH, 0).unwrap();
            let host = Registry::new(AllowAll);
            hub::add_service(&name, host.as_binder()).unwrap();
            std::thread::spawn(|| {
                wait_for_eof();
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
        other => panic!("no role {other}"),
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
}

/// The generation of the fake's `watch` reply.
const REPLY_GENERATION: i64 = 4;

/// `notify I G` sends the watch topic I with generation G; `refuse-watch` refuses the next watch.
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
        other => panic!("no command {other:?}"),
    }
}

impl Interface for Fake {}

impl ITopicRegistry for Fake {
    fn r#advertise(&self, _name: &str, topic: &Strong<dyn ITopic>) -> BinderResult<()> {
        let mut state = self.0.lock().unwrap();
        state.topics.push(topic.clone());
        say(&format!("advertised {}", state.topics.len() - 1));
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

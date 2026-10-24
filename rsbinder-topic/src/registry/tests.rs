// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The registry within one process (plan 15 AC-15.5.2, 5.3, 5.11): the table through keys and
//! links of the test's own, and the shell through binders of this process.

use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rsbinder::{BinderResult, Caller, ExceptionCode, Interface, Status, StatusCode, Strong};

use super::table::{Key, Links, Refused, Sent, Table, MAX_NAMES, MAX_OWED, MAX_WATCHERS};
use super::{classify, is_valid_name, Action, AllowAll, Registry, RegistryAccess};
use crate::aidl::ITopic::{BnTopic, ITopic, ERROR_CLOSED};
use crate::aidl::ITopicRegistry::{ITopicRegistry, ERROR_LIMIT, ERROR_TAKEN};
use crate::aidl::ITopicWatcher::{BnTopicWatcher, ITopicWatcher};
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::aidl::TopicEndpoint::TopicEndpoint;
use crate::connect::refuse;

/// Run by the shell at the places the tests judge its locks from.
pub(super) type Hook = Arc<dyn Fn() + Send + Sync>;

#[derive(Default)]
pub(super) struct Hooks {
    /// Before a change's notice is sent, `fire_order` held.
    pub before_send: Option<Hook>,
    /// Before the retry thread judges an owed notice's caller.
    pub judging: Option<Hook>,
    /// After the retry thread sent one owed notice and let `fire_order` go.
    pub between_sends: Option<Hook>,
    /// Held by the retry thread while it runs, and by these hooks: a count of 3 is a thread.
    pub alive: Option<Arc<()>>,
}

const WAIT: Duration = Duration::from_secs(10);

fn caller(uid: u32) -> Caller {
    Caller::Kernel {
        uid,
        pid: uid as i32 + 1000,
        sid: None,
    }
}

fn eventually(what: &str, done: impl Fn() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

// ---- the table ----

/// A binder as the table sees it: `id` tells binders apart, `handle` is its proxy's, if any.
#[derive(Clone, Debug, PartialEq)]
struct K {
    id: u32,
    handle: Option<u32>,
}

impl Key for K {
    fn handle(&self) -> Option<u32> {
        self.handle
    }
}

fn key(id: u32) -> K {
    K {
        id,
        handle: Some(id),
    }
}

#[derive(Default)]
struct Fake {
    linked: Vec<u32>,
    unlinked: Vec<u32>,
    fail: Option<u32>,
}

impl Fake {
    fn link(&mut self, handle: u32) -> BinderResult<()> {
        if self.fail == Some(handle) {
            return Err(StatusCode::DeadObject.into());
        }
        self.linked.push(handle);
        Ok(())
    }
}

impl Links<K, K> for Fake {
    fn link_topic(&mut self, handle: u32, _: &K) -> BinderResult<()> {
        self.link(handle)
    }

    fn link_watcher(&mut self, handle: u32, _: &K) -> BinderResult<()> {
        self.link(handle)
    }

    fn unlink(&mut self, handle: u32) {
        self.unlinked.push(handle);
    }
}

type T = Table<K, K>;

fn advertise(
    table: &mut T,
    links: &mut Fake,
    uid: u32,
    name: &str,
    topic: K,
) -> Result<usize, Refused> {
    let ids = (uid, uid as i32);
    let notices = table.advertise(ids, name, topic, false, Instant::now(), links)?;
    Ok(notices.len())
}

fn watch(
    table: &mut T,
    links: &mut Fake,
    name: &str,
    watcher: K,
) -> Result<(Option<K>, u64), Refused> {
    table.watch(name, watcher, caller(7), links)
}

#[test]
fn a_name_is_given_replaced_by_its_uid_and_refused_to_another() {
    let (mut table, mut links) = (T::new(), Fake::default());
    assert_eq!(advertise(&mut table, &mut links, 1, "a", key(10)), Ok(0));
    assert_eq!(table.lookup("a"), (Some(key(10)), 1));
    assert_eq!(advertise(&mut table, &mut links, 1, "a", key(11)), Ok(0));
    assert_eq!(table.lookup("a"), (Some(key(11)), 2));
    assert_eq!(
        advertise(&mut table, &mut links, 2, "a", key(12)),
        Err(Refused::Taken)
    );
    let crossed = table.advertise((2, 2), "a", key(12), true, Instant::now(), &mut links);
    assert!(crossed.is_ok());
    assert_eq!(table.lookup("a"), (Some(key(12)), 3));
    assert_eq!(table.lookup("b"), (None, 3));
}

#[test]
fn the_same_binder_advertised_again_keeps_its_owner_and_tells_no_one() {
    let (mut table, mut links) = (T::new(), Fake::default());
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    watch(&mut table, &mut links, "a", key(20)).unwrap();
    let since = table.list("")[0].since;
    // Another uid, the same binder: kept as the hub keeps a service registered again.
    assert_eq!(advertise(&mut table, &mut links, 2, "a", key(10)), Ok(0));
    let listed = table.list("");
    assert_eq!((listed[0].uid, listed[0].since), (1, since));
    assert_eq!(table.lookup("a").1, 1);
}

#[test]
fn a_change_tells_every_watcher_of_its_name_with_its_generation() {
    let (mut table, mut links) = (T::new(), Fake::default());
    watch(&mut table, &mut links, "a", key(20)).unwrap();
    watch(&mut table, &mut links, "a", key(21)).unwrap();
    watch(&mut table, &mut links, "b", key(22)).unwrap();
    let notices = table
        .advertise((1, 1), "a", key(10), false, Instant::now(), &mut links)
        .unwrap();
    let told: Vec<(u32, u64)> = notices
        .iter()
        .map(|n| (n.watcher.id, n.generation))
        .collect();
    assert_eq!(told, [(20, 1), (21, 1)]);
    assert!(notices.iter().all(|n| n.topic == key(10) && n.name == "a"));
    // A watch of a name with a topic answers it, and a second watch by the same binder is one.
    assert_eq!(
        watch(&mut table, &mut links, "a", key(20)),
        Ok((Some(key(10)), 1))
    );
    assert_eq!(table.counts().watchers, 3);
}

#[test]
fn withdraw_takes_back_only_the_callers_own_topic() {
    let (mut table, mut links) = (T::new(), Fake::default());
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    assert!(!table.withdraw(2, "a", &key(10), &mut links));
    assert!(!table.withdraw(1, "a", &key(11), &mut links));
    assert!(!table.withdraw(1, "b", &key(10), &mut links));
    assert!(table.withdraw(1, "a", &key(10), &mut links));
    assert_eq!(table.lookup("a"), (None, 2));
    assert_eq!(links.unlinked, [10]);
}

#[test]
fn each_map_is_bounded_apart() {
    let (mut table, mut links) = (T::new(), Fake::default());
    for i in 0..MAX_NAMES as u32 {
        advertise(
            &mut table,
            &mut links,
            1,
            &format!("t{i}"),
            K {
                id: i,
                handle: None,
            },
        )
        .unwrap();
    }
    let full = advertise(&mut table, &mut links, 1, "more", key(1));
    assert_eq!(full, Err(Refused::Limit));
    // Watches count apart: a full map of topics leaves room for them.
    watch(&mut table, &mut links, "w0", key(1)).unwrap();
    // A name with a watch alone is a new name for the topics.
    assert_eq!(
        advertise(&mut table, &mut links, 1, "w0", key(2)),
        Err(Refused::Limit)
    );
    // Replacing a held name is no new one.
    assert_eq!(
        advertise(
            &mut table,
            &mut links,
            1,
            "t0",
            K {
                id: 99,
                handle: None
            }
        ),
        Ok(0)
    );

    let (mut table, mut links) = (T::new(), Fake::default());
    for i in 0..MAX_NAMES as u32 {
        watch(
            &mut table,
            &mut links,
            &format!("w{i}"),
            K {
                id: i,
                handle: None,
            },
        )
        .unwrap();
    }
    assert_eq!(
        watch(&mut table, &mut links, "more", key(1)),
        Err(Refused::Limit)
    );
    assert_eq!(advertise(&mut table, &mut links, 1, "more", key(2)), Ok(0));
    for i in 1..MAX_WATCHERS as u32 {
        watch(
            &mut table,
            &mut links,
            "w0",
            K {
                id: 100_000 + i,
                handle: None,
            },
        )
        .unwrap();
    }
    assert_eq!(
        watch(&mut table, &mut links, "w0", key(5)),
        Err(Refused::Full)
    );
}

#[test]
fn an_empty_watch_list_leaves_the_map() {
    let (mut table, mut links) = (T::new(), Fake::default());
    watch(&mut table, &mut links, "a", key(20)).unwrap();
    table.unwatch("a", &[key(21)], &mut links);
    assert_eq!(table.counts().watched, 1);
    table.unwatch("a", &[key(20)], &mut links);
    assert_eq!((table.counts().watched, table.counts().watchers), (0, 0));
    // A watch that is not there ends as one that is.
    table.unwatch("b", &[key(20)], &mut links);
}

#[test]
fn a_list_is_in_order_under_its_prefix_and_of_names_with_a_topic() {
    let (mut table, mut links) = (T::new(), Fake::default());
    for name in ["s.b", "s.a", "t.a", "s"] {
        advertise(
            &mut table,
            &mut links,
            1,
            name,
            K {
                id: 0,
                handle: None,
            },
        )
        .unwrap_or_else(|_| panic!("{name}"));
    }
    watch(&mut table, &mut links, "s.c", key(20)).unwrap();
    watch(&mut table, &mut links, "s.a", key(21)).unwrap();
    let listed: Vec<(String, usize)> = table
        .list("s.")
        .into_iter()
        .map(|l| (l.name, l.watchers))
        .collect();
    assert_eq!(listed, [("s.a".to_owned(), 1), ("s.b".to_owned(), 0)]);
    assert_eq!(table.list("").len(), 4);
}

#[test]
fn a_death_link_is_one_per_handle_and_given_up_with_its_last_use() {
    let (mut table, mut links) = (T::new(), Fake::default());
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    advertise(&mut table, &mut links, 1, "b", key(10)).unwrap();
    watch(&mut table, &mut links, "a", key(20)).unwrap();
    watch(&mut table, &mut links, "b", key(20)).unwrap();
    assert_eq!(links.linked, [10, 20]);
    assert_eq!(
        table.linked().into_iter().collect::<Vec<_>>(),
        [(10, 2), (20, 2)]
    );
    advertise(&mut table, &mut links, 1, "a", key(11)).unwrap();
    assert!(links.unlinked.is_empty());
    assert!(table.withdraw(1, "b", &key(10), &mut links));
    assert_eq!(links.unlinked, [10]);
    table.unwatch("a", &[key(20)], &mut links);
    table.unwatch("b", &[key(20)], &mut links);
    assert_eq!(links.unlinked, [10, 20]);
    // A binder of this process has no handle and no link.
    advertise(
        &mut table,
        &mut links,
        1,
        "c",
        K {
            id: 30,
            handle: None,
        },
    )
    .unwrap();
    assert_eq!(links.linked, [10, 20, 11]);
}

#[test]
fn a_failed_link_changes_nothing() {
    let (mut table, mut links) = (T::new(), Fake::default());
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    links.fail = Some(11);
    let failed = advertise(&mut table, &mut links, 1, "a", key(11));
    assert!(matches!(failed, Err(Refused::Link(_))));
    assert_eq!(table.lookup("a"), (Some(key(10)), 1));
    let failed = watch(&mut table, &mut links, "a", key(11));
    assert!(matches!(failed, Err(Refused::Link(_))));
    assert_eq!(table.counts().watchers, 0);
    assert!(!table.linked().contains_key(&11));
}

#[test]
fn a_death_takes_its_topics_and_watches_and_counts_a_generation_for_each_name() {
    let (mut table, mut links) = (T::new(), Fake::default());
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    advertise(&mut table, &mut links, 1, "b", key(10)).unwrap();
    advertise(&mut table, &mut links, 1, "c", key(11)).unwrap();
    watch(&mut table, &mut links, "c", key(10)).unwrap();
    watch(&mut table, &mut links, "c", key(20)).unwrap();
    table.forget(10);
    assert_eq!(table.lookup("a"), (None, 5));
    assert_eq!(table.lookup("c"), (Some(key(11)), 3));
    assert_eq!(table.counts().watchers, 1);
    assert!(!table.linked().contains_key(&10));
    let notices = table.advertise((1, 1), "c", key(12), false, Instant::now(), &mut links);
    let told: Vec<u32> = notices.unwrap().iter().map(|n| n.watcher.id).collect();
    assert_eq!(told, [20]);
}

// ---- the table: notices owed (AC-15.5.11) ----

/// A table with `watchers` watches of "a", given to topic 10: the notices of that change.
fn watched(watchers: u32) -> (T, Fake, Instant) {
    let (mut table, mut links) = (T::new(), Fake::default());
    for w in 0..watchers {
        watch(&mut table, &mut links, "a", key(100 + w)).unwrap();
    }
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    (table, links, Instant::now())
}

#[test]
fn a_busy_failure_owes_the_notice_and_another_does_not() {
    let (mut table, _, now) = watched(3);
    assert!(table.sent("a", &key(100), 1, Sent::Busy, now));
    assert!(!table.sent("a", &key(101), 1, Sent::Failed, now));
    assert!(!table.sent("a", &key(102), 1, Sent::Delivered, now));
    assert_eq!(table.owed(), 1);
    assert_eq!(
        table.due_of("a", &key(100)),
        Some(now + Duration::from_secs(1))
    );
    let counts = table.counts();
    assert_eq!((counts.sent, counts.failed, counts.owed), (1, 2, 1));
    // Owed already: a second failure keeps the one entry.
    assert!(!table.sent("a", &key(100), 2, Sent::Busy, now));
    assert_eq!(table.owed(), 1);
}

#[test]
fn an_owed_notice_is_sent_again_ever_later_up_to_thirty_seconds() {
    let (mut table, _, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    assert!(table.due(now).is_empty());
    let mut at = now + Duration::from_secs(1);
    assert_eq!(table.due(at).len(), 1);
    let mut waits = Vec::new();
    for _ in 0..7 {
        let target = table.retry_target("a", &key(100));
        assert_eq!(target, Some((key(10), 1)));
        table.retried("a", &key(100), 1, Sent::Busy, at);
        let due = table.due_of("a", &key(100)).unwrap();
        waits.push((due - at).as_secs());
        at = due;
    }
    assert_eq!(waits, [2, 4, 8, 16, 30, 30, 30]);
    assert_eq!(table.next_due(), Some(at));
    table.retried("a", &key(100), 1, Sent::Delivered, at);
    assert_eq!((table.owed(), table.next_due()), (0, None));
    assert_eq!(table.counts().retried, 8);
}

#[test]
fn an_owed_notice_carries_the_topic_the_name_has_when_it_is_sent() {
    let (mut table, mut links, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    // A replacement's notice failed too: still one owed, now of the newer generation.
    let notices = table
        .advertise((1, 1), "a", key(11), false, now, &mut links)
        .unwrap();
    table.sent("a", &key(100), notices[0].generation, Sent::Busy, now);
    assert_eq!(table.retry_target("a", &key(100)), Some((key(11), 2)));
    // Delivered at the older generation, it is still owed the newer.
    table.retried("a", &key(100), 1, Sent::Delivered, now);
    assert_eq!(table.owed(), 1);
    table.retried("a", &key(100), 2, Sent::Delivered, now);
    assert_eq!(table.owed(), 0);
}

#[test]
fn a_change_delivered_clears_an_owed_notice_of_its_generation_or_older() {
    let (mut table, mut links, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    table.sent("a", &key(100), 0, Sent::Delivered, now);
    assert_eq!(table.owed(), 1);
    let notices = table
        .advertise((1, 1), "a", key(11), false, now, &mut links)
        .unwrap();
    table.sent("a", &key(100), notices[0].generation, Sent::Delivered, now);
    assert_eq!(table.owed(), 0);
}

#[test]
fn an_owed_notice_goes_with_its_watch_its_watcher_its_topic_or_a_failure_for_good() {
    let (mut table, mut links, now) = watched(4);
    for w in 100..104 {
        table.sent("a", &key(w), 1, Sent::Busy, now);
    }
    assert_eq!(table.owed(), 4);
    table.unwatch("a", &[key(100)], &mut links);
    table.forget(101);
    table.retried("a", &key(102), 1, Sent::Failed, now);
    assert_eq!(table.owed(), 1);
    assert!(table.withdraw(1, "a", &key(10), &mut links));
    assert_eq!((table.owed(), table.next_due()), (0, None));

    // A name that lost its topic by the time of the send: nothing to tell, nothing owed.
    let (mut table, _, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    table.forget(10);
    assert_eq!(table.owed(), 0);
    assert_eq!(table.retry_target("a", &key(100)), None);
}

#[test]
fn an_owed_notice_the_policy_refuses_is_put_off() {
    let (mut table, _, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    let at = now + Duration::from_secs(1);
    table.deferred("a", &key(100), at);
    assert_eq!(
        table.due_of("a", &key(100)),
        Some(at + Duration::from_secs(2))
    );
    assert_eq!(table.owed(), 1);
}

#[test]
fn past_the_bound_a_failure_is_counted_and_not_owed() {
    let (mut table, mut links) = (T::new(), Fake::default());
    let now = Instant::now();
    for name in 0..2 {
        for w in 0..MAX_WATCHERS as u32 {
            let watcher = K {
                id: name * 1000 + w,
                handle: None,
            };
            watch(&mut table, &mut links, &format!("n{name}"), watcher).unwrap();
        }
        let topic = K {
            id: 9000 + name,
            handle: None,
        };
        advertise(&mut table, &mut links, 1, &format!("n{name}"), topic).unwrap();
    }
    for name in 0..2 {
        for w in 0..MAX_WATCHERS as u32 {
            let watcher = K {
                id: name * 1000 + w,
                handle: None,
            };
            table.sent(&format!("n{name}"), &watcher, 1, Sent::Busy, now);
        }
    }
    let counts = table.counts();
    assert_eq!(
        (counts.owed, counts.dropped),
        (MAX_OWED, MAX_WATCHERS as u64 * 2 - MAX_OWED as u64)
    );
}

#[test]
fn a_status_is_sent_again_when_the_kernel_blames_a_lack_of_room_or_names_no_cause() {
    let failed = Status::from(StatusCode::FailedTransaction);
    let detail = |param| {
        move || {
            Ok(rsbinder::ExtendedError {
                id: 1,
                command: 0,
                param,
            })
        }
    };
    assert_eq!(classify(&failed, detail(-28)), Sent::Busy);
    assert_eq!(classify(&failed, detail(0)), Sent::Busy);
    assert_eq!(
        classify(&failed, || Err(StatusCode::InvalidOperation)),
        Sent::Busy
    );
    assert_eq!(classify(&failed, detail(-1)), Sent::Failed);
    assert_eq!(
        classify(&Status::from(StatusCode::DeadObject), detail(-28)),
        Sent::Failed
    );
    let security = Status::from(ExceptionCode::Security);
    assert_eq!(classify(&security, detail(-28)), Sent::Failed);
}

#[test]
fn a_name_is_one_to_127_bytes_of_the_service_name_characters() {
    assert!(is_valid_name("a"));
    assert!(is_valid_name("Sensors.imu-0/raw_1"));
    assert!(is_valid_name(&"a".repeat(127)));
    assert!(!is_valid_name(""));
    assert!(!is_valid_name(&"a".repeat(128)));
    for bad in [" ", "a b", "a:b", "a\0", "é", "a*"] {
        assert!(!is_valid_name(bad), "{bad:?}");
    }
}

// ---- the shell ----

struct Closed;

impl Interface for Closed {}

impl ITopic for Closed {
    fn r#subscribe(&self, _: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        Err(refuse(ERROR_CLOSED, "a test topic"))
    }
}

fn topic() -> Strong<dyn ITopic> {
    BnTopic::new_binder(Closed)
}

/// A watcher that records its notices and fails the sends the test queues failures for.
#[derive(Default)]
struct Recorder {
    heard: Mutex<Vec<(String, i64)>>,
    failures: Mutex<VecDeque<StatusCode>>,
}

impl Interface for Recorder {}

impl ITopicWatcher for Recorder {
    fn r#onAdvertised(
        &self,
        name: &str,
        _: &Strong<dyn ITopic>,
        generation: i64,
    ) -> BinderResult<()> {
        if let Some(failure) = self.failures.lock().unwrap().pop_front() {
            return Err(failure.into());
        }
        self.heard
            .lock()
            .unwrap()
            .push((name.to_owned(), generation));
        Ok(())
    }
}

/// A `Recorder` as the registry gets it, and the test's handle on it.
fn recorder() -> (Strong<dyn ITopicWatcher>, Arc<Recorder>) {
    let recorder = Arc::new(Recorder::default());
    let binder = BnTopicWatcher::new_binder(Shared(recorder.clone()));
    (binder, recorder)
}

struct Shared(Arc<Recorder>);

impl Interface for Shared {}

impl ITopicWatcher for Shared {
    fn r#onAdvertised(
        &self,
        name: &str,
        topic: &Strong<dyn ITopic>,
        generation: i64,
    ) -> BinderResult<()> {
        self.0.r#onAdvertised(name, topic, generation)
    }
}

/// Refuses `Subscribe` to the uids in its set, allows the rest.
#[derive(Clone, Default)]
struct Refusing(Arc<Mutex<HashSet<u32>>>);

impl RegistryAccess for Refusing {
    fn allows(&self, action: Action, _: &str, caller: &Caller) -> bool {
        let Caller::Kernel { uid, .. } = caller else {
            return false;
        };
        action != Action::Subscribe || !self.0.lock().unwrap().contains(uid)
    }
}

#[test]
fn a_call_within_the_process_is_refused() {
    let registry = Registry::new(AllowAll);
    let binder = registry.as_binder();
    let remote: Strong<dyn ITopicRegistry> = binder.into_interface().unwrap();
    let refused = remote.r#advertise("a", &topic()).unwrap_err();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
    assert!(remote.r#lookup("a").is_err());
    assert!(remote.r#list("").is_err());
    assert_eq!(registry.stats().topics, 0);
}

#[test]
fn a_watch_answers_the_topic_and_hears_the_next() {
    let registry = Registry::new(AllowAll);
    let shared = &registry.shared;
    let first = topic();
    shared.advertise_as(&caller(1), "a", &first).unwrap();
    let (watcher, heard) = recorder();
    let answer = shared.watch_as(&caller(2), "a", &watcher).unwrap();
    assert!(answer
        .topic
        .is_some_and(|t| t.as_binder() == first.as_binder()));
    assert_eq!(answer.generation, 1);
    assert!(
        heard.heard.lock().unwrap().is_empty(),
        "the reply, not a notice"
    );
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    assert_eq!(*heard.heard.lock().unwrap(), [("a".to_owned(), 2)]);
    let found = shared.lookup_as(&caller(3), "a");
    assert_eq!(found.generation, 2);
    let stats = registry.stats();
    assert_eq!(
        (stats.topics, stats.watchers, stats.notices_sent),
        (1, 1, 1)
    );
}

#[test]
fn the_shells_refusals_take_the_registrys_forms() {
    let registry = Registry::new(AllowAll);
    let shared = &registry.shared;
    let bad = shared
        .advertise_as(&caller(1), "a b", &topic())
        .unwrap_err();
    assert_eq!(bad.exception_code(), ExceptionCode::IllegalArgument);
    let (watcher, _) = recorder();
    let bad = shared.watch_as(&caller(1), "", &watcher).unwrap_err();
    assert_eq!(bad.exception_code(), ExceptionCode::IllegalArgument);
    assert!(shared.lookup_as(&caller(1), "a b").topic.is_none());
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    let taken = shared.advertise_as(&caller(2), "a", &topic()).unwrap_err();
    assert_eq!(taken.service_specific_error(), ERROR_TAKEN);
    let crossing = Registry::new(AllowAll).allow_cross_uid_overwrite(true);
    crossing
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    crossing
        .shared
        .advertise_as(&caller(2), "a", &topic())
        .unwrap();
    // `unwatch` asks no right and ends a watch that is not there.
    shared.unwatch_as("a", &watcher);
    let _ = ERROR_LIMIT;
}

#[test]
fn a_caller_refused_now_is_not_told_and_its_watch_stays() {
    let refusing = Refusing::default();
    let registry = Registry::new(refusing.clone());
    let shared = &registry.shared;
    let (watcher, heard) = recorder();
    shared.watch_as(&caller(2), "a", &watcher).unwrap();
    refusing.0.lock().unwrap().insert(2);
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    assert!(heard.heard.lock().unwrap().is_empty());
    assert_eq!(shared.lookup_as(&caller(2), "a").topic.map(|_| ()), None);
    assert_eq!((registry.stats().watchers, registry.stats().owed), (1, 0));
    refusing.0.lock().unwrap().clear();
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    assert_eq!(*heard.heard.lock().unwrap(), [("a".to_owned(), 2)]);
    // A refused watch is `EX_SECURITY`, as the hub's `registerForNotifications`.
    refusing.0.lock().unwrap().insert(3);
    let (other, _) = recorder();
    let refused = shared.watch_as(&caller(3), "a", &other).unwrap_err();
    assert_eq!(refused.exception_code(), ExceptionCode::Security);
}

#[test]
fn a_full_name_makes_room_from_the_watches_its_policy_now_refuses() {
    let refusing = Refusing::default();
    let registry = Registry::new(refusing.clone());
    let shared = &registry.shared;
    let mut held = Vec::new();
    for i in 0..MAX_WATCHERS as u32 {
        let (watcher, _) = recorder();
        shared
            .watch_as(&caller(100 + i % 2), "a", &watcher)
            .unwrap();
        held.push(watcher);
    }
    let (late, _) = recorder();
    let full = shared.watch_as(&caller(1), "a", &late).unwrap_err();
    assert_eq!(full.service_specific_error(), ERROR_LIMIT);
    refusing.0.lock().unwrap().insert(101);
    shared.watch_as(&caller(1), "a", &late).unwrap();
    assert_eq!(registry.stats().watchers, MAX_WATCHERS / 2 + 1);
}

#[test]
fn a_notice_is_sent_with_the_order_lock_held() {
    let registry = Registry::new(AllowAll);
    let weak = Arc::downgrade(&registry.shared);
    let held = Arc::new(AtomicBool::new(false));
    let seen = held.clone();
    registry.shared.hooks.lock().unwrap().before_send = Some(Arc::new(move || {
        let shared = weak.upgrade().unwrap();
        let tried =
            std::thread::scope(|s| s.spawn(|| shared.fire_order.try_lock().is_err()).join());
        seen.store(tried.unwrap(), Ordering::SeqCst);
    }));
    let (watcher, _) = recorder();
    registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
    registry
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    assert!(held.load(Ordering::SeqCst));
}

#[test]
fn a_notice_that_found_no_room_is_sent_again_and_the_thread_ends() {
    let registry = Registry::new(AllowAll);
    let alive = Arc::new(());
    registry.shared.hooks.lock().unwrap().alive = Some(alive.clone());
    let (watcher, heard) = recorder();
    registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
    heard
        .failures
        .lock()
        .unwrap()
        .push_back(StatusCode::FailedTransaction);
    let started = Instant::now();
    registry
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    assert_eq!(registry.stats().owed, 1);
    eventually("the owed notice delivered", || {
        !heard.heard.lock().unwrap().is_empty()
    });
    assert!(started.elapsed() >= Duration::from_secs(1));
    assert_eq!(*heard.heard.lock().unwrap(), [("a".to_owned(), 1)]);
    let stats = registry.stats();
    assert_eq!(
        (stats.owed, stats.notices_retried, stats.notices_failed),
        (0, 1, 1)
    );
    eventually("the retry thread ended", || Arc::strong_count(&alive) == 2);
}

#[test]
fn the_retry_judges_outside_the_locks_and_lets_the_order_lock_go_between_sends() {
    let registry = Registry::new(AllowAll);
    let weak = Arc::downgrade(&registry.shared);
    let free = Arc::new(Mutex::new(Vec::new()));
    let try_order =
        |at: &'static str, weak: std::sync::Weak<super::Shared>, free: Arc<Mutex<Vec<_>>>| {
            Arc::new(move || {
                let shared = weak.upgrade().unwrap();
                let both = shared.fire_order.try_lock().is_ok() && shared.inner.try_lock().is_ok();
                free.lock().unwrap().push((at, both));
            }) as Hook
        };
    {
        let mut hooks = registry.shared.hooks.lock().unwrap();
        hooks.judging = Some(try_order("judging", weak.clone(), free.clone()));
        hooks.between_sends = Some(try_order("between", weak, free.clone()));
    }
    let mut heard = Vec::new();
    for w in 0..2 {
        let (watcher, recorder) = recorder();
        recorder
            .failures
            .lock()
            .unwrap()
            .push_back(StatusCode::FailedTransaction);
        registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
        heard.push((watcher, recorder, w));
    }
    registry
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    eventually("both delivered", || registry.stats().owed == 0);
    let free = free.lock().unwrap().clone();
    assert_eq!(
        free,
        [
            ("judging", true),
            ("between", true),
            ("judging", true),
            ("between", true)
        ]
    );
}

#[test]
fn a_retry_thread_with_a_watcher_that_never_takes_its_notice_ends_with_the_registry() {
    let registry = Registry::new(AllowAll);
    let alive = Arc::new(());
    registry.shared.hooks.lock().unwrap().alive = Some(alive.clone());
    let (watcher, heard) = recorder();
    registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
    heard
        .failures
        .lock()
        .unwrap()
        .extend([StatusCode::FailedTransaction; 100]);
    registry
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    eventually("a send again", || registry.stats().notices_retried == 1);
    assert_eq!(Arc::strong_count(&alive), 3, "the thread, asleep for 2 s");
    let dropped = Instant::now();
    drop(registry);
    eventually("the retry thread ended", || Arc::strong_count(&alive) == 1);
    assert!(
        dropped.elapsed() < Duration::from_secs(1),
        "woken by the drop, not its due"
    );
}

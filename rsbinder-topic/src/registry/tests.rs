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
use super::{
    classify, is_valid_name, refused, Action, AllowAll, Registry, RegistryAccess, RegistryResolver,
};
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

#[test]
fn a_death_takes_only_what_its_handle_held() {
    let (mut table, mut links) = (T::new(), Fake::default());
    let now = Instant::now();
    advertise(&mut table, &mut links, 1, "a", key(10)).unwrap();
    advertise(&mut table, &mut links, 1, "b", key(10)).unwrap();
    advertise(&mut table, &mut links, 1, "c", key(11)).unwrap();
    for name in ["c", "d", "e"] {
        watch(&mut table, &mut links, name, key(20)).unwrap();
        watch(&mut table, &mut links, name, key(21)).unwrap();
    }
    table.sent("c", &key(20), 3, Sent::Busy, now);
    table.sent("c", &key(21), 3, Sent::Busy, now);
    let watching = |table: &T, name| -> Vec<u32> {
        let callers = table.callers_of(name);
        callers.iter().map(|(watcher, _)| watcher.id).collect()
    };

    table.forget(20);
    for name in ["c", "d", "e"] {
        assert_eq!(watching(&table, name), [21], "{name}");
    }
    assert_eq!(table.due_of("c", &key(20)), None);
    assert!(table.due_of("c", &key(21)).is_some());
    let linked: Vec<_> = table.linked().into_iter().collect();
    assert_eq!(linked, [(10, 2), (11, 1), (21, 3)]);

    table.forget(10);
    assert_eq!(table.lookup("a"), (None, 5));
    assert_eq!(table.lookup("b"), (None, 5));
    assert_eq!(table.lookup("c"), (Some(key(11)), 3));
    assert_eq!(table.owed(), 1);
    let linked: Vec<_> = table.linked().into_iter().collect();
    assert_eq!(linked, [(11, 1), (21, 3)]);
    // A death after the last use was given up finds nothing.
    table.forget(10);
    assert_eq!(table.lookup("a"), (None, 5));
    table.check();
}

#[test]
#[cfg(debug_assertions)]
#[should_panic(expected = "two keys of handle 5")]
fn two_keys_of_one_handle_under_one_name_break_the_key_contract() {
    let (mut table, mut links) = (T::new(), Fake::default());
    let twin = |id| K {
        id,
        handle: Some(5),
    };
    watch(&mut table, &mut links, "a", twin(1)).unwrap();
    let _ = watch(&mut table, &mut links, "a", twin(2));
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
fn an_owed_notice_the_policy_refuses_is_not_sent_again() {
    let (mut table, _, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    table.refused("a", &key(100), 1);
    assert_eq!(table.due_of("a", &key(100)), None);
    assert_eq!((table.owed(), table.next_due()), (0, None));
    // The watch stays, as for a refused notice of a change.
    assert_eq!(table.counts().watchers, 1);
}

#[test]
fn a_refusal_leaves_owed_a_newer_notice_that_failed_after_the_judgement() {
    let (mut table, mut links, now) = watched(1);
    table.sent("a", &key(100), 1, Sent::Busy, now);
    let judged = table.due(now + Duration::from_secs(1))[0].3;
    assert_eq!(judged, 1);
    let notices = table
        .advertise((1, 1), "a", key(11), false, now, &mut links)
        .unwrap();
    table.sent("a", &key(100), notices[0].generation, Sent::Busy, now);
    table.refused("a", &key(100), judged);
    assert_eq!(table.owed(), 1);
    table.refused("a", &key(100), notices[0].generation);
    assert_eq!(table.owed(), 0);
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
fn of_a_names_owed_notices_each_goes_alone() {
    let (mut table, mut links, now) = watched(3);
    for w in 100..103 {
        table.sent("a", &key(w), 1, Sent::Busy, now);
    }
    table.unwatch("a", &[key(100)], &mut links);
    assert_eq!((table.owed(), table.due_of("a", &key(100))), (2, None));
    table.refused("a", &key(101), 1);
    assert_eq!((table.owed(), table.due_of("a", &key(101))), (1, None));
    let at = now + Duration::from_secs(1);
    let due: Vec<u32> = table.due(at).iter().map(|due| due.1.id).collect();
    assert_eq!(due, [102]);
    table.retried("a", &key(102), 1, Sent::Failed, at);
    assert_eq!((table.owed(), table.next_due()), (0, None));
    table.check();
}

/// xorshift64: one sequence of changes per seed, with no crate for it.
struct Rng(u64);

impl Rng {
    fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }

    fn pick<'a, X>(&mut self, of: &'a [X]) -> &'a X {
        &of[self.below(of.len() as u64) as usize]
    }

    fn sent(&mut self) -> Sent {
        *self.pick(&[Sent::Delivered, Sent::Busy, Sent::Failed])
    }
}

#[test]
fn every_sequence_of_changes_keeps_the_maps_in_step() {
    let names = ["a", "b", "c", "d", "e"];
    let local = |id| K { id, handle: None };
    let topics = [key(10), key(11), key(12), key(13), local(1), local(2)];
    let mut watchers: Vec<K> = (20..28).map(key).collect();
    watchers.extend([local(3), local(4)]);
    let handles: Vec<u32> = (10..14).chain(20..28).collect();
    for seed in 1..=8u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let (mut table, mut links) = (T::new(), Fake::default());
        let mut now = Instant::now();
        for step in 0..5_000 {
            let name = *rng.pick(&names);
            let watcher = rng.pick(&watchers).clone();
            let uid = 1 + rng.below(2) as u32;
            // One older than the name's, its own, or one newer.
            let generation = (table.lookup(name).1 + rng.below(3)).saturating_sub(1);
            links.fail = (rng.below(10) == 0).then(|| *rng.pick(&handles));
            match rng.below(10) {
                0 => {
                    let (topic, cross_uid) = (rng.pick(&topics).clone(), rng.below(2) == 0);
                    let ids = (uid, uid as i32);
                    let changed = table.advertise(ids, name, topic, cross_uid, now, &mut links);
                    for notice in changed.unwrap_or_default() {
                        let sent = rng.sent();
                        table.sent(name, &notice.watcher, notice.generation, sent, now);
                    }
                }
                1 => {
                    table.withdraw(uid, name, rng.pick(&topics), &mut links);
                }
                2 => {
                    let _ = table.watch(name, watcher, caller(uid), &mut links);
                }
                3 => {
                    let evicted = [watcher, rng.pick(&watchers).clone()];
                    table.unwatch(name, &evicted, &mut links);
                }
                4 => table.forget(*rng.pick(&handles)),
                5 => {
                    let sent = rng.sent();
                    table.sent(name, &watcher, generation, sent, now);
                }
                6 => table.refused(name, &watcher, generation),
                7 => {
                    // As a retry round does, the policy's judgement aside.
                    for (name, watcher, _, _) in table.due(now) {
                        if let Some((_, generation)) = table.retry_target(&name, &watcher) {
                            let sent = rng.sent();
                            table.retried(&name, &watcher, generation, sent, now);
                        }
                    }
                }
                _ => now += Duration::from_secs(rng.below(40)),
            }
            let checked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| table.check()));
            assert!(checked.is_ok(), "seed {seed}, step {step}");
        }
    }
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

/// D14-a: a binder that cannot be linked is `IllegalState`, not the registry's own `DeadObject`.
#[test]
fn a_failed_link_is_an_illegal_state() {
    let status = refused(Refused::Link(StatusCode::DeadObject.into()));
    assert_eq!(status.exception_code(), ExceptionCode::IllegalState);
}

#[test]
fn a_resolver_takes_no_registry_of_this_process() {
    let registry = Registry::new(AllowAll);
    let refused = RegistryResolver::new(registry.as_binder()).unwrap_err();
    assert_eq!(refused.transaction_error(), StatusCode::BadValue);
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
    // Put in the table as no call can: the name rule alone makes the lookup's null.
    {
        let mut inner = shared.lock();
        let super::Inner { table, linked, .. } = &mut *inner;
        let this = &shared.this;
        let mut links = super::ShellLinks { linked, this };
        let key = super::TopicKey(topic());
        let ids = (1, 1);
        table
            .advertise(ids, "a b", key, false, Instant::now(), &mut links)
            .unwrap();
    }
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
    // `unwatch` asks no right: a caller refused now ends its watch, and one not there.
    let refusing = Refusing::default();
    let registry = Registry::new(refusing.clone());
    registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
    refusing.0.lock().unwrap().insert(2);
    registry.shared.unwatch_as("a", &watcher);
    registry.shared.unwatch_as("a", &watcher);
    assert_eq!(registry.stats().watchers, 0);
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
fn a_change_takes_the_order_lock_before_the_table() {
    let registry = Registry::new(AllowAll);
    let shared = &registry.shared;
    std::thread::scope(|s| {
        let inner = shared.lock();
        let change = s.spawn(|| shared.advertise_as(&caller(1), "a", &topic()));
        // Held by a change that waits on `inner`: the retry thread takes them in this order.
        eventually("the order lock held", || {
            shared.fire_order.try_lock().is_err()
        });
        drop(inner);
        change.join().unwrap().unwrap();
    });
}

#[test]
fn a_refused_lookup_answers_no_topic_older_than_any_change() {
    let refusing = Refusing::default();
    let registry = Registry::new(refusing.clone());
    let shared = &registry.shared;
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    shared.advertise_as(&caller(1), "b", &topic()).unwrap();
    refusing.0.lock().unwrap().insert(2);
    let refused = shared.lookup_as(&caller(2), "a");
    let none = shared.lookup_as(&caller(3), "c");
    assert!(refused.topic.is_none() && none.topic.is_none());
    assert_eq!((refused.generation, none.generation), (i64::MIN, 2));
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
    // Counted after the watcher heard it, under the lock `owed` is cleared under.
    eventually("the owed notice delivered", || {
        registry.stats().notices_retried == 1
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
fn an_owed_notice_the_retry_thread_finds_refused_is_not_sent_and_the_thread_ends() {
    let refusing = Refusing::default();
    let registry = Registry::new(refusing.clone());
    let alive = Arc::new(());
    let mut hooks = registry.shared.hooks.lock().unwrap();
    hooks.alive = Some(alive.clone());
    // After the change's judgement allowed it: the retry thread's judgement refuses.
    let refuse = refusing.clone();
    hooks.before_send = Some(Arc::new(move || {
        refuse.0.lock().unwrap().insert(2);
    }));
    drop(hooks);
    let (watcher, heard) = recorder();
    registry.shared.watch_as(&caller(2), "a", &watcher).unwrap();
    heard
        .failures
        .lock()
        .unwrap()
        .push_back(StatusCode::FailedTransaction);
    registry
        .shared
        .advertise_as(&caller(1), "a", &topic())
        .unwrap();
    eventually("the owed notice refused", || registry.stats().owed == 0);
    assert!(heard.heard.lock().unwrap().is_empty());
    let stats = registry.stats();
    assert_eq!((stats.notices_retried, stats.watchers), (0, 1));
    eventually("the retry thread ended", || Arc::strong_count(&alive) == 2);
}

/// Allows every action, and panics on judging `Subscribe` while armed.
#[derive(Clone, Default)]
struct Panicking(Arc<AtomicBool>);

impl RegistryAccess for Panicking {
    fn allows(&self, action: Action, _: &str, _: &Caller) -> bool {
        let armed = action == Action::Subscribe && self.0.load(Ordering::SeqCst);
        assert!(!armed, "the test's judgement panics");
        true
    }
}

#[test]
fn a_change_whose_judgement_panicked_leaves_the_next_to_go_on() {
    let panicking = Panicking::default();
    let registry = Registry::new(panicking.clone());
    let shared = &registry.shared;
    let (watcher, heard) = recorder();
    shared.watch_as(&caller(2), "a", &watcher).unwrap();
    panicking.0.store(true, Ordering::SeqCst);
    let changed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        shared.advertise_as(&caller(1), "a", &topic())
    }));
    assert!(changed.is_err());
    panicking.0.store(false, Ordering::SeqCst);
    // The panic was under the order lock: the next change takes it all the same.
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    assert_eq!(*heard.heard.lock().unwrap(), [("a".to_owned(), 2)]);
}

#[test]
fn a_retry_thread_whose_judgement_panicked_leaves_the_next_send_to_start_another() {
    let panicking = Panicking::default();
    let registry = Registry::new(panicking.clone());
    let shared = &registry.shared;
    let alive = Arc::new(());
    let (stuck, heard) = recorder();
    let (other, _) = recorder();
    shared.watch_as(&caller(2), "a", &stuck).unwrap();
    shared.watch_as(&caller(2), "b", &other).unwrap();
    heard
        .failures
        .lock()
        .unwrap()
        .push_back(StatusCode::FailedTransaction);
    let mut hooks = shared.hooks.lock().unwrap();
    hooks.alive = Some(alive.clone());
    // After the change's judgement: the retry thread's panics.
    let arm = panicking.clone();
    hooks.before_send = Some(Arc::new(move || arm.0.store(true, Ordering::SeqCst)));
    drop(hooks);
    shared.advertise_as(&caller(1), "a", &topic()).unwrap();
    eventually("a retry thread", || Arc::strong_count(&alive) == 3);
    eventually("the retry thread ended", || Arc::strong_count(&alive) == 2);
    assert_eq!(registry.stats().owed, 1);

    shared.hooks.lock().unwrap().before_send = None;
    panicking.0.store(false, Ordering::SeqCst);
    // Delivered, so it owes nothing new: it starts a thread for what was owed.
    shared.advertise_as(&caller(1), "b", &topic()).unwrap();
    eventually("the owed notice delivered", || {
        registry.stats().notices_retried == 1
    });
    assert_eq!(*heard.heard.lock().unwrap(), [("a".to_owned(), 1)]);
}

type Free = Arc<Mutex<Vec<(&'static str, bool)>>>;

fn both_free(shared: &super::Shared) -> bool {
    shared.fire_order.try_lock().is_ok() && shared.inner.try_lock().is_ok()
}

/// Allows everything, and records whether both locks are free as the retry thread judges.
struct Judging {
    shared: Arc<std::sync::OnceLock<std::sync::Weak<super::Shared>>>,
    free: Free,
    /// Set once the test's change returned: the retry thread records no lock that change holds.
    changed: Arc<AtomicBool>,
}

impl RegistryAccess for Judging {
    fn allows(&self, _: Action, _: &str, _: &Caller) -> bool {
        let retrying = std::thread::current().name() == Some("rsbinder-topic-registry");
        let shared = self.shared.get().and_then(std::sync::Weak::upgrade);
        if let Some(shared) = shared.filter(|_| retrying) {
            eventually("the change returned", || {
                self.changed.load(Ordering::SeqCst)
            });
            self.free
                .lock()
                .unwrap()
                .push(("judging", both_free(&shared)));
        }
        true
    }
}

#[test]
fn the_retry_judges_outside_the_locks_and_lets_the_order_lock_go_between_sends() {
    let free: Free = Arc::default();
    let judging = Judging {
        shared: Arc::default(),
        free: free.clone(),
        changed: Arc::default(),
    };
    let set = judging.shared.clone();
    let changed = judging.changed.clone();
    let registry = Registry::new(judging);
    set.set(Arc::downgrade(&registry.shared)).unwrap();
    let weak = Arc::downgrade(&registry.shared);
    let between = free.clone();
    registry.shared.hooks.lock().unwrap().between_sends = Some(Arc::new(move || {
        let shared = weak.upgrade().unwrap();
        between
            .lock()
            .unwrap()
            .push(("between", both_free(&shared)));
    }));
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
    changed.store(true, Ordering::SeqCst);
    // The thread's last act for a send, and no wait on `inner`, which the hooks try.
    eventually("both delivered", || free.lock().unwrap().len() == 4);
    assert_eq!(registry.stats().owed, 0);
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

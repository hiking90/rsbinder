// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The release of the writer lock and a subscriber's drop notice of `publish.rs` under loom
//! (plan 15-pubsub-gaps §12): a copy of `Held`'s drop, `TypedWriter::unsubscribed`,
//! `settle_owed`, `list_changed` and `TypedWriter::wait_subscribers` with no deadline, over a
//! list of subscriptions whose control pages are atomic `CLOSED` flags. The copy holds every
//! decision those functions make: the flag is written with a `Release` RMW and a `SeqCst` fence
//! follows; `settle_owed` returns on a `try_lock` that finds the lock held and, on one that takes
//! it, prunes only when its `AcqRel` swap takes the flag; every release of the lock, the one of
//! the settle's `try_lock` too, is the drop, which takes `changed`, lets go, counts a change in
//! `changes` and wakes, fences, and settles again when it finds the flag up, so a settle can
//! recurse; a wait reads `changes`, looks under the lock, releases through the drop, and sleeps
//! on `changes` only while it is the value it read. A poisoned lock and the deadline are left
//! out: a poison panics every later lock, and without a deadline `timed_out` is never set.
//!
//! The threads are a wait, the drop of a subscription, and either another holder of the lock (a
//! `give_back`, which changes nothing of the list) or the drop of a second subscription, whose
//! settle is the holder another drop's `try_lock` finds. What loom checks is that no wake is
//! lost: a wait that sleeps while a drop it has not seen is owed, with nothing left to wake it,
//! is a deadlock, which loom reports. A subscriber's drop never takes the writer lock with
//! `lock`, which panics on its thread. The flawed models show the checks are not vacuous: a
//! holder that lets go with the plain guard, a wait that sleeps on the state lock's condvar, a
//! wait or a settle whose release skips the second look at the flag, a wait that sleeps without
//! comparing `changes`, one that reads `changes` after its look, a change that wakes without
//! counting, a flag written with a plain store, a settle that waits for the lock, and the fence
//! and ordering ones below.
//!
//! Loom's `Mutex` synchronizes as acquire and release only, as std's does (loom 0.7's `seq_cst`
//! on a lock and an unlock does nothing), and a failed `try_lock` synchronizes with nothing.
//! Its `SeqCst` fence joins one global order of every `SeqCst` fence before it, which is
//! stronger than C++'s fence rules, so an execution that only those weaker rules allow is not
//! explored: the fences are argued in `publish.rs`, not proven here. A fence or an ordering
//! taken out still fails, since nothing else orders a drop's flag against a holder's look.
//!
//! The copy is kept to the source by
//! [`loom_publish_the_copy_matches_the_decisions_of_publish_rs`]: it fails when `publish.rs`
//! takes the state lock other than in `lock` and `settle_owed`, the two that wrap it in `Held`,
//! or when a step the copy holds is gone from its function, commented out, or out of its order
//! there. It reads the source without comments, whitespace and a comma before `)`, so a rewrap
//! by rustfmt, a trailing comma of arguments split one per line included, passes, and a change
//! of spelling fails; the copy is then brought to the new code, and the step to the copy. Code
//! added between the steps, such as another release of the lock or a binding declared again
//! under a step's name, is not caught: that is the review's to see.
//! Run: see `Cargo.toml` (`--cfg loom`).

use std::cell::Cell;
use std::ops::{Deref, DerefMut};
// The std `Arc`: one of loom's dropped while a deadlock unwinds calls into loom and aborts.
use std::sync::{Arc, TryLockError};

use loom::sync::atomic::{fence, AtomicBool, Ordering};
use loom::sync::{Condvar, Mutex, MutexGuard};
use loom::thread;

/// Subscriptions a model starts with.
const SUBSCRIPTIONS: usize = 2;

/// What a model leaves out of the protocol.
#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// The other holder lets the lock go with the plain guard, around the drop's second look.
    PlainRelease,
    /// The wait sleeps on a condvar that lets the state lock go, not on `changes`.
    SleepsOnState,
    /// The wait's own release skips the second look at the flag.
    WaitSkipsLook,
    /// The settle's release skips the second look at the flag: no settle recurses.
    SettleSkipsLook,
    /// The wait sleeps without comparing `changes` with the value it read.
    NoCompare,
    /// The wait reads `changes` after its look, not before.
    CountsAfterLook,
    /// A change of the list wakes without counting in `changes`.
    NoCount,
    /// The flag is written with a plain store: another drop's store ends the first's sequence.
    StoredFlag,
    /// The settle takes the lock with `lock`, waiting for its holder.
    SettleLocks,
    /// The drop loads the flag with no fence before.
    NoDropFence,
    /// `unsubscribed` tries the lock with no fence after the flag.
    NoNoticeFence,
    /// The drop's fence is `AcqRel`.
    AcqRelDropFence,
    /// The settle swaps the flag `Relaxed`.
    RelaxedSettle,
    /// The drop notice swaps the flag `Relaxed`.
    RelaxedFlag,
}

loom::thread_local! {
    /// Raised while a subscriber's drop runs: `lock` panics in it.
    static DROPPING: Cell<bool> = Cell::new(false);
}

/// The part of `State` the protocol reads: `Links` with its generation and `changed`.
struct List {
    linked: [bool; SUBSCRIPTIONS],
    generation: u64,
    changed: bool,
}

impl List {
    /// `Links::prune`: the subscriptions taken out; a prune that takes one bumps the generation.
    fn prune(&mut self, closed: &[AtomicBool; SUBSCRIPTIONS]) -> usize {
        let mut gone = 0;
        for (linked, closed) in self.linked.iter_mut().zip(closed) {
            // `Link::ended` loads the page's state `Relaxed`.
            if *linked && closed.load(Ordering::Relaxed) {
                *linked = false;
                gone += 1;
            }
        }
        if gone > 0 {
            self.generation += 1;
            self.changed = true;
        }
        gone
    }

    fn count(&self) -> usize {
        self.linked.iter().filter(|&&linked| linked).count()
    }
}

/// `TypedWriter`'s fields of the protocol.
struct Topic {
    state: Mutex<List>,
    /// Each subscription's control page, its `CLOSED` bit.
    closed: [AtomicBool; SUBSCRIPTIONS],
    subscribers: Condvar,
    changes: Mutex<u64>,
    prune_owed: AtomicBool,
    flaw: Flaw,
}

/// `Held`: the lock, held; every release is its drop.
struct Hold<'a> {
    topic: &'a Topic,
    /// `None` only inside the drop.
    state: Option<MutexGuard<'a, List>>,
    /// Whether the drop looks at the flag again; `false` only in a flawed model.
    looks: bool,
}

impl Deref for Hold<'_> {
    type Target = List;

    fn deref(&self) -> &List {
        self.state.as_ref().expect("held until dropped")
    }
}

impl DerefMut for Hold<'_> {
    fn deref_mut(&mut self) -> &mut List {
        self.state.as_mut().expect("held until dropped")
    }
}

impl Drop for Hold<'_> {
    fn drop(&mut self) {
        let Some(mut state) = self.state.take() else {
            return;
        };
        let topic = self.topic;
        let changed = std::mem::take(&mut state.changed);
        drop(state);
        if changed {
            topic.list_changed();
        }
        if !self.looks {
            return;
        }
        match topic.flaw {
            Flaw::NoDropFence => {}
            Flaw::AcqRelDropFence => fence(Ordering::AcqRel),
            _ => fence(Ordering::SeqCst),
        }
        if topic.prune_owed.load(Ordering::Relaxed) {
            topic.settle_owed();
        }
    }
}

impl Topic {
    fn new(flaw: Flaw) -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(List {
                linked: [true; SUBSCRIPTIONS],
                generation: 0,
                changed: false,
            }),
            closed: [(); SUBSCRIPTIONS].map(|()| AtomicBool::new(false)),
            subscribers: Condvar::new(),
            changes: Mutex::new(0),
            prune_owed: AtomicBool::new(false),
            flaw,
        })
    }

    /// `TypedWriter::lock`.
    fn lock(&self) -> Hold<'_> {
        let dropping = DROPPING.with(Cell::get);
        assert!(!dropping, "a subscriber's drop waits for the writer lock");
        self.held(self.state.lock().unwrap())
    }

    fn held<'a>(&'a self, state: MutexGuard<'a, List>) -> Hold<'a> {
        Hold {
            topic: self,
            state: Some(state),
            looks: true,
        }
    }

    /// `Closing::drop`: mark the page `CLOSED`, then `TypedWriter::unsubscribed`.
    fn drop_subscription(&self, nth: usize) {
        DROPPING.with(|dropping| dropping.set(true));
        self.closed[nth].fetch_or(true, Ordering::Release);
        match self.flaw {
            Flaw::StoredFlag => self.prune_owed.store(true, Ordering::Release),
            Flaw::RelaxedFlag => {
                self.prune_owed.swap(true, Ordering::Relaxed);
            }
            _ => {
                self.prune_owed.swap(true, Ordering::Release);
            }
        }
        if self.flaw != Flaw::NoNoticeFence {
            fence(Ordering::SeqCst);
        }
        self.settle_owed();
        DROPPING.with(|dropping| dropping.set(false));
    }

    /// `TypedWriter::settle_owed`.
    fn settle_owed(&self) {
        let mut state = if self.flaw == Flaw::SettleLocks {
            self.lock()
        } else {
            match self.state.try_lock() {
                Ok(state) => self.held(state),
                Err(TryLockError::WouldBlock) => return,
                Err(TryLockError::Poisoned(_)) => panic!("lock poisoned"),
            }
        };
        state.looks = self.flaw != Flaw::SettleSkipsLook;
        let order = match self.flaw {
            Flaw::RelaxedSettle => Ordering::Relaxed,
            _ => Ordering::AcqRel,
        };
        if self.prune_owed.swap(false, order) {
            state.prune(&self.closed);
        }
    }

    /// `TypedWriter::list_changed`.
    fn list_changed(&self) {
        let mut changes = self.changes.lock().unwrap();
        if self.flaw != Flaw::NoCount {
            *changes = changes.wrapping_add(1);
        }
        drop(changes);
        self.subscribers.notify_all();
    }

    /// `TypedWriter::wait_subscribers` with no deadline: the generation and the count.
    fn wait_subscribers(&self, seen: u64) -> (u64, usize) {
        if self.flaw == Flaw::SleepsOnState {
            return self.wait_on_state(seen);
        }
        loop {
            let mut counted = *self.changes.lock().unwrap();
            let mut state = self.lock();
            let gone = state.prune(&self.closed);
            let now = (state.generation, state.count());
            let done = now.0 != seen || gone > 0;
            state.looks = self.flaw != Flaw::WaitSkipsLook;
            drop(state);
            if done {
                return now;
            }
            if self.flaw == Flaw::CountsAfterLook {
                counted = *self.changes.lock().unwrap();
            }
            let changes = self.changes.lock().unwrap();
            if *changes != counted && self.flaw != Flaw::NoCompare {
                continue;
            }
            drop(self.subscribers.wait(changes).unwrap());
        }
    }

    /// [`Flaw::SleepsOnState`]'s wait: the condvar lets the state lock go around the drop.
    fn wait_on_state(&self, seen: u64) -> (u64, usize) {
        let mut state = self.lock();
        loop {
            let gone = state.prune(&self.closed);
            let now = (state.generation, state.count());
            if now.0 != seen || gone > 0 {
                return now;
            }
            let guard = state.state.take().expect("held until dropped");
            state.state = Some(self.subscribers.wait(guard).unwrap());
        }
    }

    /// Another holder: a `give_back`, which changes nothing of the list.
    fn hold(&self) {
        let mut held = self.lock();
        // The work under the lock: loom switches only at such a point, and a release is none.
        thread::yield_now();
        if self.flaw == Flaw::PlainRelease {
            drop(held.state.take());
        }
    }
}

fn check(test: impl Fn() + Sync + Send + 'static) {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(test);
}

/// One subscription of two dropped while a wait looks and another thread holds the lock.
fn a_drop_while_another_holds_the_lock_wakes_the_wait(flaw: Flaw) {
    check(move || {
        let topic = Topic::new(flaw);
        let subscriber = {
            let topic = topic.clone();
            thread::spawn(move || topic.drop_subscription(0))
        };
        let holder = {
            let topic = topic.clone();
            thread::spawn(move || topic.hold())
        };
        assert_eq!(
            topic.wait_subscribers(0),
            (1, 1),
            "the wait ended without the drop"
        );
        subscriber.join().unwrap();
        holder.join().unwrap();
    });
}

#[test]
fn loom_publish_a_drop_while_another_holds_the_lock_wakes_the_wait() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_holder_that_lets_go_with_the_plain_guard_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::PlainRelease);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_wait_that_sleeps_on_the_state_lock_sleeps_through() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::SleepsOnState);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_wait_whose_release_skips_the_flag_sleeps_through() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::WaitSkipsLook);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_wait_that_does_not_compare_changes_sleeps_through() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::NoCompare);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_wait_that_reads_changes_after_its_look_sleeps_through() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::CountsAfterLook);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_change_that_wakes_without_counting_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::NoCount);
}

#[test]
#[should_panic(expected = "a subscriber's drop waits for the writer lock")]
fn loom_publish_a_settle_that_takes_the_lock_waits_in_the_drop() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::SettleLocks);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_release_without_its_fence_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::NoDropFence);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_notice_without_its_fence_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::NoNoticeFence);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_release_with_an_acq_rel_fence_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::AcqRelDropFence);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_settle_that_swaps_relaxed_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::RelaxedSettle);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_notice_that_swaps_relaxed_leaves_the_wait_asleep() {
    a_drop_while_another_holds_the_lock_wakes_the_wait(Flaw::RelaxedFlag);
}

/// Both subscriptions dropped at once while a wait for none left runs: a settle can recurse.
fn two_drops_at_once_wake_the_wait_until_both_are_seen(flaw: Flaw) {
    check(move || {
        let topic = Topic::new(flaw);
        let subscribers = [0, 1].map(|nth| {
            let topic = topic.clone();
            thread::spawn(move || topic.drop_subscription(nth))
        });
        let mut seen = 0;
        loop {
            let (generation, count) = topic.wait_subscribers(seen);
            if count == 0 {
                break;
            }
            seen = generation;
        }
        for subscriber in subscribers {
            subscriber.join().unwrap();
        }
    });
}

#[test]
fn loom_publish_two_drops_at_once_wake_the_wait_until_both_are_seen() {
    two_drops_at_once_wake_the_wait_until_both_are_seen(Flaw::None);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_settle_that_does_not_recurse_leaves_the_second_drop_owed() {
    two_drops_at_once_wake_the_wait_until_both_are_seen(Flaw::SettleSkipsLook);
}

#[test]
#[should_panic(expected = "deadlock")]
fn loom_publish_a_flag_stored_plain_lets_a_settle_miss_the_other_drop() {
    two_drops_at_once_wake_the_wait_until_both_are_seen(Flaw::StoredFlag);
}

/// `text` without comments, whitespace and a comma before `)`: the same however rustfmt wraps it.
fn flat(text: &str) -> String {
    // No string of `publish.rs` holds `//` or `/*`, so each starts a comment.
    let mut code = String::new();
    let mut rest = text;
    while let Some(at) = rest.find("/*") {
        code.push_str(&rest[..at]);
        let end = rest[at..].find("*/").expect("a block comment ends");
        rest = &rest[at + end + 2..];
    }
    code.push_str(rest);
    let code: String = code
        .lines()
        .map(|line| line.split("//").next().unwrap_or_default())
        .flat_map(str::split_whitespace)
        .collect();
    code.replace(",)", ")")
}

/// Each function of `publish.rs` the copy holds: where it starts, what follows it, its steps.
const STEPS: [(&str, &str, &[&str]); 7] = [
    (
        "pub(crate) fn lock(&self)",
        "fn held<'a>",
        &["self.held(self.state.lock()"],
    ),
    (
        "impl<T: Payload, S: Stores> Drop for Held<'_, T, S>",
        "pub(crate) struct TypedWriter<T, S>",
        &[
            "let changed = state.links.take_changed();",
            "drop(state);",
            "if changed { topic.list_changed(); }",
            "fence(Ordering::SeqCst);",
            "if topic.prune_owed.load(Ordering::Relaxed) { topic.settle_owed(); }",
        ],
    ),
    (
        "pub(crate) fn unsubscribed(&self)",
        "fn settle_owed(&self)",
        &[
            "self.prune_owed.swap(true, Ordering::Release);",
            "fence(Ordering::SeqCst);",
            "self.settle_owed();",
        ],
    ),
    (
        "fn settle_owed(&self)",
        "fn list_changed(&self)",
        &[
            "match self.state.try_lock() {",
            "Err(TryLockError::WouldBlock) => return,",
            "let mut state = self.held(state);",
            "if self.prune_owed.swap(false, Ordering::AcqRel) { state.links.prune() }",
            "self.unlock(state, gone);",
        ],
    ),
    (
        "fn list_changed(&self)",
        "pub(crate) fn subscribers(&self)",
        &[
            "*changes = changes.wrapping_add(1);",
            "drop(changes);",
            "self.subscribers.wake_all();",
        ],
    ),
    (
        "pub(crate) fn wait_subscribers(",
        "async fn wait_subscribers_async(",
        &[
            "let counted = *self.changes.lock()",
            "let mut state = self.lock();",
            "let gone = state.links.prune();",
            "let done = timed_out || now.generation != seen || !gone.is_empty();",
            "self.unlock(state, gone);",
            "if done { return now; }",
            "let changes = self.changes.lock()",
            "if *changes != counted { continue; }",
            ".wait_until(changes, deadline)",
        ],
    ),
    (
        "pub(crate) fn unlock(",
        "pub(crate) fn wakes(&self)",
        &["state.gone.extend(gone); drop(state);"],
    ),
];

/// Every hold of `publish.rs` is a `Held`, and each step the copy holds is there, in its order.
#[test]
fn loom_publish_the_copy_matches_the_decisions_of_publish_rs() {
    let source = flat(include_str!("../publish.rs"));
    let count = |text: &str| source.matches(&flat(text)).count();
    assert_eq!(
        count(".state.lock("),
        1,
        "a lock of the state outside `lock`"
    );
    assert_eq!(
        count(".state.try_lock("),
        1,
        "a try of the state outside `settle_owed`"
    );
    for (start, end, steps) in STEPS {
        assert_eq!(count(start), 1, "`publish.rs` has not one `{start}`");
        let from = source.find(&flat(start)).expect("counted");
        let to = source[from..]
            .find(&flat(end))
            .unwrap_or_else(|| panic!("nothing follows `{start}` as `{end}`"));
        let body = &source[from..from + to];
        let mut at = 0;
        for step in steps {
            let step = flat(step);
            let Some(found) = body[at..].find(&step) else {
                panic!("`{start}` has not, after the steps before it: {step}");
            };
            at += found + step.len();
        }
    }
}

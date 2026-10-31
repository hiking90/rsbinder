// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher that waits for its subscribers to make room (plan 15 §6.5).
//!
//! A record of a topic with [`Backpressure`] is written only once it overwrites no record a
//! subscriber still has to read: with `E` where the record ends, once `E − capacity` is at or
//! below the lowest `read` the subscriptions report. With a [`Backpressure::window`] of `w` that
//! `read` has also to be at least where the record `w - 1` before it starts
//! (`Writer::window_floor` in `ring.rs`), so that no subscriber is more than `w` records behind.
//! The publisher waits for that under the topic's lock, looking for a spin's budget and then
//! sleeping, each sleep twice the last up to [`MAX_NAP`] and back to [`MIN_NAP`] whenever a
//! subscriber reads on. A subscriber stores its `read` with `Release` once it has copied the
//! record, and the publisher loads it with `Acquire`, so the record's bytes are read before they
//! are written over.
//!
//! A lend from the topic's pool waits the same way, for a slot that every subscription has read
//! past and none pins (`pool.rs`) and for the window of the record the loan will publish, and
//! past the timeout takes out the subscriptions furthest behind, one at a time, until there is
//! one.

use std::num::NonZeroUsize;
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::spin::{spin_until, SPIN};

/// How a topic waits for its slowest subscriber: [`PublisherPolicy::backpressure`].
///
/// A publish waits until the record it writes overwrites no record that a subscriber has not
/// read yet, from where the subscriber's subscription started on, and a loan
/// ([`Publisher::loan_slice`]) waits for a slot that every subscriber has read past. With a
/// [`window`](Self::window) both also wait until the record leaves no subscriber more than that
/// many records behind. A subscriber that never falls that far behind costs nothing; one that
/// does holds the publisher, and with it every other subscriber, for up to
/// [`publish_timeout`](Self::publish_timeout), and is then taken out.
///
/// # What waits with it
///
/// - The wait holds the topic's lock. Subscriptions that arrive meanwhile, [`Publisher::stats`],
///   [`Publisher::close`] and the publishes of other threads wait for the lock, and the timeout
///   of each counts from when it holds the lock: a publish queued behind another one's wait can
///   take longer than `publish_timeout`. [`Publisher::publish_all`] counts the timeout per
///   value, so it takes up to the number of values times `publish_timeout`.
/// - [`Publisher::close`], and dropping the publisher, write the end record after the same wait,
///   so that a subscriber reads every value before the end: a drop can take up to
///   `publish_timeout`.
/// - The wait sleeps the calling thread. From async code, publish on a blocking thread
///   (`tokio::task::spawn_blocking`, or a thread of its own).
/// - A thread that publishes and is also the topic's only slow subscriber, receiving between
///   its publishes, waits for itself and takes itself out after `publish_timeout`.
///
/// # Binder threads
///
/// A subscriber's death ends a wait for it once a binder thread of the publisher's process has
/// read the death notice: the publisher's process runs the binder thread pool. A subscription
/// call that arrives during a wait holds a binder thread until the wait ends, so subscribers
/// that keep retrying can hold every thread of the pool, and the wait then ends by its timeout
/// rather than by the death.
///
/// A subscriber learns that it was taken out through a one-way call into its process, which its
/// binder thread pool handles. One that runs no pool is not told: it goes on receiving and
/// loses the records written over, as [`RecvError::Lagged`].
///
/// # Subscribing again
///
/// A subscriber taken out may subscribe again, and if it is as slow as before it holds each
/// publish up for `publish_timeout` again before it is taken out again. The topic does not
/// refuse it: limit who may subscribe with [`PublisherPolicy::authorize`], and how many
/// subscriptions one uid holds with [`PublisherPolicy::max_per_uid`].
///
/// [`PublisherPolicy::backpressure`]: crate::PublisherPolicy::backpressure
/// [`PublisherPolicy::authorize`]: crate::PublisherPolicy::authorize
/// [`PublisherPolicy::max_per_uid`]: crate::PublisherPolicy::max_per_uid
/// [`Publisher::loan_slice`]: crate::Publisher::loan_slice
/// [`Publisher::stats`]: crate::Publisher::stats
/// [`Publisher::close`]: crate::Publisher::close
/// [`Publisher::publish_all`]: crate::Publisher::publish_all
/// [`RecvError::Lagged`]: crate::RecvError::Lagged
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backpressure {
    /// The longest a publish waits for room, from when it holds the topic's lock. Default 1 s.
    /// The same bound holds a loan ([`Publisher::loan_slice`](crate::Publisher::loan_slice))
    /// that waits for a slot, a wait for the [`window`](Self::window), and the wait of
    /// [`Publisher::close`](crate::Publisher::close), or of dropping the publisher, to write the
    /// end record.
    ///
    /// It is the publisher's timeout, not a subscriber's: a subscriber that stops reading is
    /// taken out only once it holds up a publish, a loan or the end record, and then after this
    /// timeout. Until then nothing waits for it, and the time it can stay stopped before it is
    /// taken out is the time the publisher takes to fill the ring, or the window, behind it,
    /// which the publish rate decides, plus this timeout (plan 15 #2).
    ///
    /// Once it passes, the subscriptions in the way are taken out and the publish goes on: each
    /// subscriber is told, its receives return
    /// [`Closed::Evicted`](crate::Closed::Evicted), and the publisher logs it and counts it in
    /// [`PublisherStats::evicted`](crate::PublisherStats::evicted). A `publish_timeout` of zero
    /// takes them out at once; one past what an `Instant` holds waits without a bound.
    pub publish_timeout: Duration,
    /// The most records a subscriber may be behind the publisher, counting the one it is reading:
    /// a record that would leave one further behind waits. `None` (the default) lets it fall as
    /// far behind as the ring holds.
    ///
    /// A subscriber that falls behind without one lets the ring fill. Each value it receives then
    /// waited behind every record before it, and between the record it reads and the newest lie
    /// the bytes written since: once those are more than the CPU's last-level cache holds, it
    /// reads its records from DRAM. A `window` bounds both by a number of records, whatever the
    /// machine. Set one for large values with several subscribers, or where a value has to
    /// arrive within a bound; 2 lets the publisher write one record while the slowest
    /// subscriber reads the one before. A loaned value received as a [`Sample`](crate::Sample) is
    /// being read until the sample is dropped.
    ///
    /// On the ring a window moves a cost rather than removing it. The publisher writes where the
    /// ring's oldest record lies: in a ring of `r` records with a window of `w` it writes over
    /// bytes read `r - w` records before, which may have left the CPU's caches by then, where a
    /// full ring without a window writes over the record just read. With one subscriber that
    /// reads about as fast as the publisher writes, that can lower the rate. A ring of about
    /// `w + 1` records ([`PublisherPolicy::for_payload`](crate::PublisherPolicy::for_payload)
    /// with a depth of `w`) keeps the writes where reading just ended. A pool has no such cost:
    /// a loan takes the slot read most recently.
    ///
    /// Waiting for the window is waiting for room, with the same `publish_timeout`: a subscriber
    /// that stops for longer is taken out though the ring has room left. A small window leaves
    /// little to ride out a subscriber's stall with, and on a topic of small values published at
    /// a high rate the publisher stalls with it; choose `publish_timeout` longer than the stalls
    /// to ride out. A
    /// window larger than the records the ring holds waits as `None` does.
    pub window: Option<NonZeroUsize>,
}

impl Default for Backpressure {
    fn default() -> Self {
        Self {
            // Longer than a scheduling delay, shorter than a stop a person makes (plan 15 §6.5).
            publish_timeout: Duration::from_secs(1),
            window: None,
        }
    }
}

/// The shortest sleep of a wait for room, and the one after a subscriber reads on.
pub(crate) const MIN_NAP: Duration = Duration::from_micros(50);

/// The longest sleep: also the longest a publisher takes to see room it waited for (§6.5).
pub(crate) const MAX_NAP: Duration = Duration::from_millis(1);

/// The subscriptions a wait for room looks at.
pub(crate) trait Readers {
    /// The lowest read position of those the publisher waits for; `u64::MAX` with none.
    fn lowest_read(&self) -> u64;
}

/// How a wait for room ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Room {
    /// Every subscription waited for has read up to the position asked for.
    Made,
    /// The deadline passed first.
    TimedOut,
}

/// Wait until `readers` have read up to `need`, or `deadline` passes; `None` waits without one.
pub(crate) fn wait_for(readers: &impl Readers, need: u64, deadline: Option<Instant>) -> Room {
    wait_until(readers, || readers.lowest_read() >= need, deadline)
}

/// Wait until `made` holds, or `deadline` passes: a pool's wait for a slot, which a pin let go
/// frees as well as a read. Each rise of `readers`' lowest read starts the sleeps over.
pub(crate) fn wait_until(
    readers: &impl Readers,
    mut made: impl FnMut() -> bool,
    deadline: Option<Instant>,
) -> Room {
    if made() {
        return Room::Made;
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Room::TimedOut;
    }
    if spin_until(SPIN, &mut made).0.is_found() {
        return Room::Made;
    }
    let mut nap = MIN_NAP;
    let mut seen = readers.lowest_read();
    loop {
        if made() {
            return Room::Made;
        }
        let read = readers.lowest_read();
        if read > seen {
            seen = read;
            nap = MIN_NAP;
        }
        let now = Instant::now();
        let sleep = match deadline {
            Some(deadline) if now >= deadline => return Room::TimedOut,
            Some(deadline) => nap.min(deadline - now),
            None => nap,
        };
        std::thread::sleep(sleep);
        nap = (nap * 2).min(MAX_NAP);
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;

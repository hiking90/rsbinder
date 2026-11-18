// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher that waits for its subscribers to make room (plan 15 §6.5).
//!
//! A record of a [`lossless`](crate::PublisherBuilder::lossless) topic is written only once it
//! overwrites no record a subscriber still has to read: with `E` where the record ends, once
//! `E − capacity` is at or below the lowest `read` the subscriptions report. With a
//! [`window`](crate::PublisherBuilder::lossless_window) of `w` that
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

/// How a lossless topic waits for its slowest subscriber: the settings of
/// [`PublisherBuilder::lossless`](crate::PublisherBuilder::lossless) and
/// [`lossless_window`](crate::PublisherBuilder::lossless_window).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Lossless {
    /// The longest a publish, a loan or the end record waits for room.
    pub(crate) timeout: Duration,
    /// The most records a subscriber may be behind; `None` lets it fall as far as the ring holds.
    pub(crate) window: Option<NonZeroUsize>,
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

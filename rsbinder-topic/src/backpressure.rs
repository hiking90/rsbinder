// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher that waits for its subscribers to make room (plan 15 §6.5).
//!
//! A record of a topic with [`Backpressure`] is written only once it overwrites no record a
//! subscriber still has to read: with `E` where the record ends, once `E − capacity` is at or
//! below the lowest `read` the subscriptions report. The publisher waits for that under the
//! topic's lock, looking for a spin's budget and then sleeping, each sleep twice the last up to
//! [`MAX_NAP`] and back to [`MIN_NAP`] whenever a subscriber reads on. A subscriber stores its
//! `read` with `Release` once it has copied the record, and the publisher loads it with `Acquire`,
//! so the record's bytes are read before they are written over.

use std::time::{Duration, Instant};

use rsbinder::fmq::shm::spin::{spin_until, SPIN};

/// How a topic waits for its slowest subscriber: [`PublisherPolicy::backpressure`].
///
/// A publish waits until the record it writes overwrites no record that a subscriber has not
/// read yet, from where the subscriber's subscription started on. A subscriber that never falls
/// behind by a ring's worth costs nothing; one that does holds the publisher, and with it every
/// other subscriber, for up to `timeout`.
///
/// [`PublisherPolicy::backpressure`]: crate::PublisherPolicy::backpressure
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Backpressure {
    /// The longest a publish waits for room, from when it holds the topic's lock. Default 1 s.
    ///
    /// Once it passes the record is written anyway, and the subscribers in the way lose the
    /// records it writes over, as on a topic without backpressure. A `timeout` past what an
    /// `Instant` holds waits without one.
    pub timeout: Duration,
}

impl Default for Backpressure {
    fn default() -> Self {
        Self {
            // Longer than a scheduling delay, shorter than a stop a person makes (plan 15 §6.5).
            timeout: Duration::from_secs(1),
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
    let made = || readers.lowest_read() >= need;
    if made() {
        return Room::Made;
    }
    if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
        return Room::TimedOut;
    }
    if spin_until(SPIN, made).0 {
        return Room::Made;
    }
    let mut nap = MIN_NAP;
    let mut seen = readers.lowest_read();
    loop {
        let read = readers.lowest_read();
        if read >= need {
            return Room::Made;
        }
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

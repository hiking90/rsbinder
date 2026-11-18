// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The builders a topic's two ends are made with (plan 15 field §3.3, §3.6).
//!
//! A publisher builder sizes the ring from the largest value and the records to keep, and holds
//! only the settings that fit the kind of topic it makes: the kind is the builder's type
//! parameter, chosen by the call that starts it. A subscriber builder picks the target in its
//! last call, which also decides the type it returns. Neither keeps a value or a closure of the
//! caller's but [`authorize`](PublisherBuilder::authorize)'s check, so both are `Clone`: one
//! setting, several topics.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::time::Duration;

use rsbinder::{BinderResult, CallingContext, ExceptionCode, SIBinder, Status};

use crate::backpressure::Lossless;
use crate::publish::{ring_for, Authorize, PoolSpec, Spec};
use crate::resolve::{Resolver, ServiceManager};
use crate::subscriber::Settings;
use crate::{NamedSubscriber, Payload, Publisher, Start, Subscriber, Wake};

/// The kind of a topic whose records are events: each one happened once, and a subscriber that
/// fell behind is told how many it lost. The type parameter of a [`PublisherBuilder`] that
/// [`Publisher::events`] starts.
#[derive(Debug)]
pub enum EventTopic {}

/// A builder of a [`Publisher`] of `T`, for a topic of the kind `K`: [`Publisher::events`].
///
/// Setting the same thing twice keeps the last call. The settings that only one kind of topic
/// has, and the call that makes the publisher, are in the `impl` block of that kind.
///
/// ```no_run
/// use std::time::Duration;
/// use rsbinder_topic::{Bytes, Publisher};
///
/// # fn main() -> rsbinder::BinderResult<()> {
/// let readings = Publisher::<Bytes>::events()
///     .max_value_bytes(256)
///     .keep(100)
///     .lossless(Duration::from_secs(1))
///     .build()?;
/// # drop(readings);
/// # Ok(()) }
/// ```
#[must_use = "a builder makes no topic until its last call"]
pub struct PublisherBuilder<T, K> {
    max_value_bytes: Option<usize>,
    keep: usize,
    ring_bytes: Option<usize>,
    max_subscribers: usize,
    max_per_uid: usize,
    authorize: Option<Authorize>,
    lend: Option<usize>,
    max_loans: Option<usize>,
    lend_keep: Option<usize>,
    huge_pages: bool,
    lossless: Option<Lossless>,
    _topic: PhantomData<fn() -> (T, K)>,
}

impl<T, K> Clone for PublisherBuilder<T, K> {
    fn clone(&self) -> Self {
        Self {
            max_value_bytes: self.max_value_bytes,
            keep: self.keep,
            ring_bytes: self.ring_bytes,
            max_subscribers: self.max_subscribers,
            max_per_uid: self.max_per_uid,
            authorize: self.authorize.clone(),
            lend: self.lend,
            max_loans: self.max_loans,
            lend_keep: self.lend_keep,
            huge_pages: self.huge_pages,
            lossless: self.lossless.clone(),
            _topic: PhantomData,
        }
    }
}

impl<T, K> fmt::Debug for PublisherBuilder<T, K> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PublisherBuilder")
            .field("max_value_bytes", &self.max_value_bytes)
            .field("keep", &self.keep)
            .field("ring_bytes", &self.ring_bytes)
            .field("max_subscribers", &self.max_subscribers)
            .field("max_per_uid", &self.max_per_uid)
            .field("authorize", &self.authorize)
            .field("lend", &self.lend)
            .field("max_loans", &self.max_loans)
            .field("lend_keep", &self.lend_keep)
            .field("huge_pages", &self.huge_pages)
            .field("lossless", &self.lossless)
            .finish()
    }
}

impl<T: Payload + 'static> Publisher<T> {
    /// A builder of an event topic: one whose records each tell of something that happened once,
    /// so that a subscriber that falls behind is told how many it lost
    /// ([`RecvError::Lagged`](crate::RecvError::Lagged)).
    pub fn events() -> PublisherBuilder<T, EventTopic> {
        PublisherBuilder::new()
    }
}

impl<T: Payload + 'static, K> PublisherBuilder<T, K> {
    fn new() -> Self {
        let spec = Spec::default();
        Self {
            max_value_bytes: None,
            keep: 1,
            ring_bytes: None,
            max_subscribers: spec.max_subscribers,
            max_per_uid: spec.max_per_uid,
            authorize: None,
            lend: None,
            max_loans: None,
            lend_keep: None,
            huge_pages: false,
            lossless: None,
            _topic: PhantomData,
        }
    }

    /// The most bytes a value encodes to, which sizes the ring. Without it the ring is the
    /// smallest, 4 KiB, whose records carry at most 2040 bytes.
    ///
    /// A record is at most half the ring, its payload at most `ring_bytes / 2 - 8` bytes; a
    /// value encoded past that fails to publish with
    /// [`PublishError::TooLarge`](crate::PublishError::TooLarge). The ring is one memfd the
    /// publisher allocates up front and every subscriber maps, so it is charged once, to the
    /// publisher, however many subscribers there are. A value lent from the pool
    /// ([`lend`](Self::lend)) goes into a slot, and its ring record is 24 bytes whatever its size:
    /// `max_value_bytes` sizes the records of [`publish`](Publisher::publish) alone.
    pub fn max_value_bytes(mut self, bytes: usize) -> Self {
        self.max_value_bytes = Some(bytes);
        self
    }

    /// Subscriptions the topic holds at once. Default 64.
    ///
    /// The publisher maps each subscriber's control page and, after every commit, loads a word of
    /// it, so the bound limits both the mappings a subscriber can make this process hold and the
    /// work of a publish. One more is refused with [`ITopic::ERROR_FULL`]; subscriptions that
    /// ended (the subscriber dropped it, or its process died) are taken out first. With 0 every
    /// subscription is refused so, and a [`NamedSubscriber`] retries `ERROR_FULL` for as long as
    /// it lives; to refuse callers for good, use [`authorize`](Self::authorize), whose refusal it
    /// does not retry.
    ///
    /// A publish wakes the subscribers it finds waiting before it lets go of the topic's lock,
    /// so the bound is also the longest a publish holds it and the other publishers wait. The
    /// wake grows with the subscribers that wait: one futex wake for all those of
    /// [`Wake::Futex`], whose kernel time grows with each one it wakes, and one `send` for each
    /// of [`Wake::Fd`]. Measured on an x86_64 desktop (plan 15 §8.4), a publish took about
    /// 0.25 µs with no subscriber waiting, and with 64 waiting about 0.17 ms on futex wakes and
    /// 0.28 ms on fd wakes: about 1–3 µs and 4.5 µs for each subscriber that waits. Lower the
    /// bound where a publish must not stall that long, raise it for more subscribers at that
    /// cost. A pool ([`lend`](Self::lend)) has a slot per subscription.
    ///
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    pub fn max_subscribers(mut self, max: usize) -> Self {
        self.max_subscribers = max;
        self
    }

    /// Subscriptions one uid holds at once, refused past it as
    /// [`max_subscribers`](Self::max_subscribers) is. Default 64, which with the default
    /// `max_subscribers` limits nothing: lower it so that one uid allowed to subscribe cannot take
    /// every place. With 0 every subscription is refused, as with a `max_subscribers` of 0.
    pub fn max_per_uid(mut self, max: usize) -> Self {
        self.max_per_uid = max;
        self
    }

    /// Who may subscribe: the callers for which `admit` returns `true`. Without it any caller
    /// that can reach the topic's binder, which for a topic registered with the service manager
    /// is any caller it lets find the name.
    ///
    /// Checked first for every subscription, from the publisher's own binder
    /// ([`Publisher::as_binder`]) and from an [`Acceptor`](crate::Acceptor) alike, with the caller
    /// of the subscription call, on the binder thread the call arrived on while it waits. A
    /// refusal is `EX_SECURITY`, which a [`NamedSubscriber`] does not retry on the same
    /// publisher. A subscriber in the publisher's process is seen with the process's own uid and
    /// pid.
    pub fn authorize(
        mut self,
        admit: impl Fn(&CallingContext) -> bool + Send + Sync + 'static,
    ) -> Self {
        self.authorize = Some(Authorize::new(admit));
        self
    }

    /// Lend values from a pool of slots of `slot_bytes` each, rounded up to a multiple of 64: the
    /// largest value a loan holds (plan 15 §6). Without it every value goes through the ring.
    ///
    /// A loaned value ([`Publisher::loan_slice`]) is written straight into a slot of a second
    /// memfd that every subscriber maps read-only, and the ring carries only a record of 24 bytes
    /// that names the slot. A subscriber pins the slot while it reads it, and the publisher never
    /// lends a pinned slot, so a value larger than a cache, which costs its size twice to copy
    /// through the ring, is written once.
    ///
    /// The pool has `max_subscribers + 1 + keep + max_loans` slots (with the `keep` of
    /// `lend_keep`): every subscriber pinning one still leaves a slot to lend, with the values
    /// kept for subscribers that have not pinned them yet. All of its pages are written when the
    /// topic is made and stay the publisher's memory: the default 64 subscribers with 1 MiB slots
    /// are 68 MiB of the publisher's resident memory, whether or not anyone subscribes. Lower
    /// [`max_subscribers`](Self::max_subscribers) for a pool of large slots. A pool larger than
    /// 4 GiB is refused.
    pub fn lend(mut self, slot_bytes: usize) -> Self {
        self.lend = Some(slot_bytes);
        self
    }

    /// Loans held at once and not yet published, past which a loan is refused with
    /// [`PublishError::TooManyLoans`](crate::PublishError::TooManyLoans). Default 1; 0 counts as
    /// 1. Only with [`lend`](Self::lend): without it the last call refuses the topic.
    pub fn max_loans(mut self, max: usize) -> Self {
        self.max_loans = Some(max);
        self
    }

    /// Ask the kernel for transparent huge pages for the pool (`MADV_HUGEPAGE`). Only with
    /// [`lend`](Self::lend): without it the last call refuses the topic.
    ///
    /// Copying out of a slot then takes fewer TLB misses: on an x86_64 desktop with 4 KiB pages
    /// otherwise, a 1 MiB value copied about 6–10 µs faster (plan 15 §8.4). The pool's resident
    /// memory then grows in steps of 2 MiB. Where the system turns huge pages for shared memory
    /// off (`/sys/kernel/mm/transparent_hugepage/shmem_enabled` is `never` or `deny`), it changes
    /// nothing.
    pub fn lend_huge_pages(mut self) -> Self {
        self.huge_pages = true;
        self
    }

    /// Bytes of ring, from 4 KiB to 1 GiB, rounded up to a multiple of 8, in place of the size
    /// [`max_value_bytes`](Self::max_value_bytes) and the records to keep work out. For a ring of
    /// a size chosen elsewhere: one to compare with another transport's, or a small one that a
    /// test laps. [`Publisher::ring_bytes`] reads the size a topic was made with.
    pub fn ring_bytes(mut self, bytes: usize) -> Self {
        self.ring_bytes = Some(bytes);
        self
    }

    /// `depth` records of `max_value_bytes` in the ring, `lend_keep` values in the pool.
    fn spec(&self, depth: usize, lend_keep: usize) -> BinderResult<Spec> {
        if self.lend.is_none() {
            let stray = [
                (self.max_loans.is_some(), "max_loans"),
                (self.lend_keep.is_some(), "lend_keep"),
                (self.huge_pages, "lend_huge_pages"),
            ];
            if let Some((_, setting)) = stray.iter().find(|(set, _)| *set) {
                let why =
                    format!("`{setting}` is a setting of the pool, and `lend` was not called");
                return Err(Status::from((ExceptionCode::IllegalArgument, why.as_str())));
            }
        }
        let ring_bytes = self
            .ring_bytes
            .unwrap_or_else(|| ring_for(self.max_value_bytes.unwrap_or(0), depth));
        Ok(Spec {
            ring_bytes,
            max_subscribers: self.max_subscribers,
            max_per_uid: self.max_per_uid,
            authorize: self.authorize.clone(),
            pool: self.lend.map(|slot_bytes| PoolSpec {
                slot_bytes,
                depth: lend_keep,
                max_loans: self.max_loans.unwrap_or(1),
                huge_pages: self.huge_pages,
            }),
            backpressure: self.lossless.clone(),
        })
    }
}

impl<T: Payload + 'static> PublisherBuilder<T, EventTopic> {
    /// Records of [`max_value_bytes`](Self::max_value_bytes) a subscriber can fall behind by
    /// before it loses the oldest, besides the one being written. Default 1; 0 counts as 1.
    ///
    /// The ring is `(keep + 1) × (max_value_bytes rounded up to 8, plus 8)` bytes, and no less
    /// than 4 KiB, so smaller records leave room for more. Without `max_value_bytes` the count
    /// holds for values of up to 8 bytes only. Past 1 GiB the last call refuses the topic.
    pub fn keep(mut self, records: usize) -> Self {
        self.keep = records;
        self
    }

    /// The newest values lent from the pool that stay readable for a subscriber behind. Default
    /// 2; 0 counts as 1. Only with [`lend`](Self::lend): without it the last call refuses the
    /// topic.
    ///
    /// Each publish takes back the values older than these once its own value is readable, so a
    /// subscriber that falls more than `values` behind loses the older ones, as one that falls
    /// behind the ring does, and receives no value older than `values` publishes.
    /// [`Subscriber::try_latest`] goes on to the newer value instead. A value a subscriber holds
    /// as a [`Sample`](crate::Sample) stays its own however old it grows. The slot lent next is
    /// one that every subscriber has read past: the one published second last of those, or the
    /// one published last when it is the only one; with none, the oldest. A slot that has held no
    /// value counts as read past and as published before every other. A busy pool thus reuses
    /// the slots still in the cache but not the one its subscribers read last, whose lines their
    /// cores still hold. Passing that one over saves at most what a subscriber core's private
    /// cache holds of the value, and only for a subscriber that reads the value whole; with
    /// values from about a fifth to three-eighths of the last-level cache upward, by how the
    /// value is read, the two slots written in turn and the buffers copied from and into no
    /// longer stay in it, and each fill reads part of the slot from memory instead (plan 15).
    ///
    /// With [`lossless`](Self::lossless) no value is taken back: a loan waits instead for a slot
    /// every subscriber has read past, and a subscriber can fall at least `values + 1` values
    /// behind before a loan waits for it.
    pub fn lend_keep(mut self, values: usize) -> Self {
        self.lend_keep = Some(values);
        self
    }

    /// Wait for the slowest subscriber, up to `timeout`, instead of overwriting what it has not
    /// read. Without it (or [`lossless_window`](Self::lossless_window)) nothing waits, and a
    /// subscriber that falls more than the ring or [`lend_keep`](Self::lend_keep) loans behind
    /// loses the oldest values ([`RecvError::Lagged`](crate::RecvError::Lagged)).
    ///
    /// A publish waits until the record it writes overwrites no record that a subscriber has not
    /// read yet, from where the subscriber's subscription started on, and a loan
    /// ([`Publisher::loan_slice`]) waits for a slot that every subscriber has read past. A
    /// subscriber that never falls that far behind costs nothing; one that does holds the
    /// publisher, and with it every other subscriber, for up to `timeout`, and is then taken out.
    ///
    /// `timeout` is the longest a publish waits for room, from when it holds the topic's lock.
    /// The same bound holds a loan that waits for a slot, a wait for the window of
    /// [`lossless_window`](Self::lossless_window), and the wait of [`Publisher::close`], or of
    /// dropping the publisher, to write the end record. It is the publisher's timeout, not a
    /// subscriber's: a subscriber that stops reading is taken out only once it holds up a
    /// publish, a loan or the end record, and then after this timeout. Until then nothing waits
    /// for it, and the time it can stay stopped before it is taken out is the time the publisher
    /// takes to fill the ring, or the window, behind it, which the publish rate decides, plus
    /// this timeout (plan 15 #2). Once it passes, the subscriptions in the way are taken out and
    /// the publish goes on: each subscriber is told, its receives return
    /// [`Closed::Evicted`](crate::Closed::Evicted), and the publisher logs it and counts it in
    /// [`PublisherStats::evicted`](crate::PublisherStats::evicted). A `timeout` of zero takes
    /// them out at once; one past what an `Instant` holds waits without a bound. A second for a
    /// `timeout` is longer than a scheduling delay and shorter than a stop a person makes
    /// (plan 15 §6.5).
    ///
    /// # What waits with it
    ///
    /// - The wait holds the topic's lock. Subscriptions that arrive meanwhile,
    ///   [`Publisher::stats`], [`Publisher::close`] and the publishes of other threads wait for
    ///   the lock, and the timeout of each counts from when it holds the lock: a publish queued
    ///   behind another one's wait can take longer than `timeout`. [`Publisher::publish_all`]
    ///   counts the timeout per value, so it takes up to the number of values times `timeout`.
    /// - [`Publisher::close`], and dropping the publisher, write the end record after the same
    ///   wait, so that a subscriber reads every value before the end: a drop can take up to
    ///   `timeout`.
    /// - The wait sleeps the calling thread. From async code, publish on a blocking thread
    ///   (`tokio::task::spawn_blocking`, or a thread of its own).
    /// - A thread that publishes and is also the topic's only slow subscriber, receiving between
    ///   its publishes, waits for itself and takes itself out after `timeout`.
    ///
    /// # Binder threads
    ///
    /// A subscriber's death ends a wait for it once a binder thread of the publisher's process
    /// has read the death notice: the publisher's process runs the binder thread pool. A
    /// subscription call that arrives during a wait holds a binder thread until the wait ends, so
    /// subscribers that keep retrying can hold every thread of the pool, and the wait then ends
    /// by its timeout rather than by the death.
    ///
    /// A subscriber learns that it was taken out through a one-way call into its process, which
    /// its binder thread pool handles. One that runs no pool is not told: it goes on receiving
    /// and loses the records written over, as [`RecvError::Lagged`](crate::RecvError::Lagged).
    ///
    /// # Subscribing again
    ///
    /// A subscriber taken out may subscribe again, and if it is as slow as before it holds each
    /// publish up for `timeout` again before it is taken out again. The topic does not refuse
    /// it: limit who may subscribe with [`authorize`](Self::authorize), and how many
    /// subscriptions one uid holds with [`max_per_uid`](Self::max_per_uid).
    pub fn lossless(mut self, timeout: Duration) -> Self {
        self.lossless = Some(Lossless {
            timeout,
            window: None,
        });
        self
    }

    /// [`lossless`](Self::lossless), and a publish or a loan waits as well until its record
    /// leaves no subscriber more than `window` records behind, counting the one it is reading.
    ///
    /// A subscriber that falls behind without a window lets the ring fill. Each value it receives
    /// then waited behind every record before it, and between the record it reads and the newest
    /// lie the bytes written since: once those are more than the CPU's last-level cache holds, it
    /// reads its records from DRAM. A window bounds both by a number of records, whatever the
    /// machine. Set one for large values with several subscribers, or where a value has to
    /// arrive within a bound; 2 lets the publisher write one record while the slowest subscriber
    /// reads the one before. A loaned value received as a [`Sample`](crate::Sample) is being read
    /// until the sample is dropped.
    ///
    /// On the ring a window moves a cost rather than removing it. The publisher writes where the
    /// ring's oldest record lies: in a ring of `r` records with a window of `w` it writes over
    /// bytes read `r - w` records before, which may have left the CPU's caches by then, where a
    /// full ring without a window writes over the record just read. With one subscriber that
    /// reads about as fast as the publisher writes, that can lower the rate. A ring of about
    /// `w + 1` records ([`keep`](Self::keep) of `w`) keeps the writes where reading just ended. A
    /// pool has no such cost: a loan takes the slot read most recently.
    ///
    /// Waiting for the window is waiting for room, with the same `timeout`: a subscriber that
    /// stops for longer is taken out though the ring has room left. A small window leaves little
    /// to ride out a subscriber's stall with, and on a topic of small values published at a high
    /// rate the publisher stalls with it; choose a `timeout` longer than the stalls to ride out.
    /// A window larger than the records the ring holds waits as `lossless` does.
    pub fn lossless_window(mut self, timeout: Duration, window: NonZeroUsize) -> Self {
        self.lossless = Some(Lossless {
            timeout,
            window: Some(window),
        });
        self
    }

    /// Make the topic.
    ///
    /// `EX_ILLEGAL_ARGUMENT` when the ring is outside 4 KiB to 1 GiB, the pool is larger than
    /// 4 GiB, or a setting of the pool was given without [`lend`](Self::lend); the status's
    /// message names the check that failed. `EX_UNSUPPORTED_OPERATION` off Linux and Android, on
    /// a target where a 64-bit word of a read-only mapping cannot be loaded, and on a kernel
    /// without `F_SEAL_FUTURE_WRITE`. The status code of the OS error when the memory cannot be
    /// made (`NoMemory` for `ENOMEM`).
    pub fn build(self) -> BinderResult<Publisher<T>> {
        Publisher::from_spec(&self.event_spec()?)
    }

    pub(crate) fn event_spec(&self) -> BinderResult<Spec> {
        self.spec(self.keep, self.lend_keep.unwrap_or(2))
    }
}

/// A builder of a [`Subscriber`] or a [`NamedSubscriber`] of `T`: [`Subscriber::builder`].
///
/// The settings come first, the target last; the last call decides what is made. Setting the
/// same thing twice keeps the last call.
///
/// ```no_run
/// use rsbinder::hub;
/// use rsbinder_topic::{Bytes, Start, Subscriber, Wake};
///
/// # fn main() -> Result<(), Box<dyn std::error::Error>> {
/// let builder = Subscriber::<Bytes>::builder().start_at(Start::Newest).wake(Wake::Fd);
///
/// let topic = hub::check_service("example.topic").ok_or("no topic")?;
/// let one = builder.clone().subscribe(&topic)?;
/// let named = builder.subscribe_named("example.other")?;
/// # drop((one, named));
/// # Ok(()) }
/// ```
#[must_use = "a builder subscribes to nothing until its last call"]
pub struct SubscriberBuilder<T> {
    settings: Settings,
    _payload: PhantomData<fn() -> T>,
}

impl<T> Clone for SubscriberBuilder<T> {
    fn clone(&self) -> Self {
        Self {
            settings: self.settings.clone(),
            _payload: PhantomData,
        }
    }
}

impl<T> fmt::Debug for SubscriberBuilder<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("SubscriberBuilder")
            .field(&self.settings)
            .finish()
    }
}

impl<T: Payload> SubscriberBuilder<T> {
    pub(crate) fn new() -> Self {
        Self {
            settings: Settings::default(),
            _payload: PhantomData,
        }
    }

    /// Where each connection starts reading. Default [`Start::Next`].
    pub fn start_at(mut self, start: Start) -> Self {
        self.settings.start = start;
        self
    }

    /// How the publisher wakes this subscriber. Default [`Wake::Futex`].
    pub fn wake(mut self, wake: Wake) -> Self {
        self.settings.wake = wake;
        self
    }

    /// The largest ring this subscriber maps, in bytes. Default 16 MiB.
    ///
    /// The publisher chooses the ring's size and the subscriber maps all of it read-only, so
    /// this bounds the address space a publisher can make the subscriber's process give up. A
    /// larger ring is refused with `BadValue`.
    pub fn max_ring_bytes(mut self, bytes: usize) -> Self {
        self.settings.max_ring_bytes = bytes;
        self
    }

    /// The largest pool this subscriber maps, in bytes. Default 256 MiB.
    ///
    /// A publisher that lends slots ([`PublisherBuilder::lend`]) sends a pool besides the ring,
    /// and the subscriber maps all of it read-only, so this bounds the address space it can make
    /// the subscriber's process give up as [`max_ring_bytes`](Self::max_ring_bytes) does. A
    /// larger pool is refused with `BadValue`.
    pub fn max_pool_bytes(mut self, bytes: usize) -> Self {
        self.settings.max_pool_bytes = bytes;
        self
    }

    /// Subscribe to the topic behind `topic`, an `ITopic` binder: a kernel binder proxy, a binder
    /// of this process, or an RPC binder of a Unix-socket session that passes fds and has an
    /// incoming connection. The subscriber does not vouch for the publisher
    /// ([`Untrusted`](crate::Untrusted)).
    ///
    /// One twoway call; after it, receiving makes none. The refusals of the publisher come back
    /// as it sent them ([`Acceptor::accept`](crate::Acceptor::accept) lists them, and an
    /// application's `ITopic` may refuse with `EX_SECURITY`). Of its own, `subscribe` returns
    /// `InvalidOperation` for an RPC binder of a live session with no callback connection
    /// (`TransportCaps::CALLBACKS`: one the publisher can call on, whichever end opened it), which
    /// the publisher could not tell of an eviction, `DeadObject` for one of a session that has
    /// ended, what the parcel answers to the fds of a session that passes none,
    /// `EX_UNSUPPORTED_OPERATION` where topics cannot work (off Linux and Android, or on a kernel
    /// without `F_SEAL_FUTURE_WRITE`),
    /// [`ITopic::ERROR_VERSION`](crate::aidl::ITopic::ERROR_VERSION) for a publisher of another
    /// layout version, and `BadValue` for a reply it cannot use: topic memory that is not a
    /// sealed memfd of the expected layout, a ring larger than
    /// [`max_ring_bytes`](Self::max_ring_bytes), or a pool that is not a sealed memfd of the
    /// slots it states or is larger than [`max_pool_bytes`](Self::max_pool_bytes).
    ///
    /// **The process has to run the binder thread pool**
    /// (`rsbinder::ProcessState::start_thread_pool`) for a publisher in another process: the
    /// notice of its death arrives on a pool thread, and without it a subscriber never learns
    /// that its publisher died, and a wait for a record never ends. Over RPC the notice arrives
    /// through the session's incoming connection instead, and the caller keeps the session open
    /// for as long as the subscriber.
    pub fn subscribe(self, topic: &SIBinder) -> BinderResult<Subscriber<T>> {
        let start = self.settings.start;
        Subscriber::request_starting(topic, &self.settings, |_| (start, None))
    }

    /// Subscribe to the topic of the publisher at `endpoint`, with
    /// [`Options`](crate::uds::Options)' defaults.
    ///
    /// The returned [`Subscriber`] holds the session and closes it as it drops, so nothing is left
    /// to end. Its errors are those of [`subscribe`](Self::subscribe) and of connecting: the code
    /// of the kernel's answer (`NameNotFound` for no file, `Errno(-ECONNREFUSED)` for no listener,
    /// `Errno(-EACCES)` for the socket file's mode) and `TimedOut` past
    /// [`Options::timeout`](crate::uds::Options::timeout).
    #[cfg(feature = "uds")]
    pub fn connect(self, endpoint: crate::uds::Endpoint) -> BinderResult<Subscriber<T>> {
        self.connect_with(endpoint, &crate::uds::Options::default())
    }

    /// [`connect`](Self::connect) with `options`.
    #[cfg(feature = "uds")]
    pub fn connect_with(
        self,
        endpoint: crate::uds::Endpoint,
        options: &crate::uds::Options,
    ) -> BinderResult<Subscriber<T>> {
        crate::uds::connect_with(endpoint, &self.settings, options)
    }
}

impl<T: Payload + 'static> SubscriberBuilder<T> {
    /// Subscribe to the topic the service manager gives `name` to, now or later.
    ///
    /// Returns once the name is watched (`rsbinder::hub::watch`), without waiting for a
    /// publisher. Fails when the watch does: a name the service manager refuses, or Android
    /// 10's service manager, which has no registration notices. The settings are used for every
    /// connection, but for where each connection after the first starts (see
    /// [`NamedSubscriber`]).
    pub fn subscribe_named(self, name: &str) -> BinderResult<NamedSubscriber<T>> {
        self.subscribe_named_on(name, &ServiceManager)
    }

    /// [`subscribe_named`](Self::subscribe_named) through `resolver` instead of the service
    /// manager.
    pub fn subscribe_named_on(
        self,
        name: &str,
        resolver: &dyn Resolver,
    ) -> BinderResult<NamedSubscriber<T>> {
        NamedSubscriber::subscribe_with(name, self.settings, resolver)
    }
}

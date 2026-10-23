// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The publisher's typed side: what a topic is made with, and values encoded into records.

use std::fmt;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::os::fd::OwnedFd;
use std::sync::atomic::{fence, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::Mapping;
use rsbinder::{CallingContext, Status, StatusCode};

use crate::backpressure::{wait_for, wait_until, Room};
use crate::layout::{self, LentSlot, Stored, StoredPool, MIN_RING};
use crate::link::{Eviction, Link, Links};
use crate::payload::{relabel, Piece};
use crate::pool::{LendError, Loan, Slots, MAX_PINS};
use crate::ring::{Kind, Refused, Stores, Writer};
use crate::{end, Backpressure, Encoding, Payload, PublisherStats};

/// What a topic is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherPolicy {
    /// Bytes of ring, from 4 KiB to 1 GiB, rounded up to a multiple of 8. Default 64 KiB.
    ///
    /// The ring is one memfd the publisher allocates up front and every subscriber maps, so it
    /// is charged once, to the publisher, however many subscribers there are. A record is at most
    /// half the ring: the payload of one is at most `ring_bytes / 2 - 8` bytes, 32760 in the
    /// default ring. How many records a subscriber can fall behind by before it loses some is the
    /// ring's size over the size of a record; [`for_payload`](Self::for_payload) works the size
    /// out from that.
    pub ring_bytes: usize,
    /// Subscriptions the topic holds at once. Default 64.
    ///
    /// The publisher maps each subscriber's control page and, after every commit, loads a word of
    /// it, so the bound limits both the mappings a subscriber can make this process hold and the
    /// work of a publish. One more is refused with [`ITopic::ERROR_FULL`]; subscriptions that
    /// ended (the subscriber dropped it, or its process died) are taken out first.
    ///
    /// A publish wakes the subscribers it finds waiting before it lets go of the topic's lock,
    /// so the bound is also the longest a publish holds it and the other publishers wait. The
    /// wake grows with the subscribers that wait: one futex wake for all those of
    /// [`Wake::Futex`], whose kernel time grows with each one it wakes, and one `send` for each
    /// of [`Wake::Fd`]. Measured on an x86_64 desktop (plan 15 §8.4), a publish took about
    /// 0.25 µs with no subscriber waiting, and with 64 waiting about 0.17 ms on futex wakes and
    /// 0.28 ms on fd wakes: about 1–3 µs and 4.5 µs for each subscriber that waits. Lower the
    /// bound where a publish must not stall that long, raise it for more subscribers at that
    /// cost.
    ///
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    /// [`Wake::Futex`]: crate::Wake::Futex
    /// [`Wake::Fd`]: crate::Wake::Fd
    pub max_subscribers: usize,
    /// Subscriptions one uid holds at once, refused past it as `max_subscribers` is. Default 64,
    /// which with the default `max_subscribers` limits nothing: lower it so that one uid allowed
    /// to subscribe cannot take every place.
    pub max_per_uid: usize,
    /// Who may subscribe. Default `None`: any caller that can reach the topic's binder, which
    /// for a topic registered with the service manager is any caller it lets find the name.
    ///
    /// Checked first for every subscription, from the publisher's own binder
    /// ([`Publisher::as_binder`](crate::Publisher::as_binder)) and from an
    /// [`Acceptor`](crate::Acceptor) alike, with the caller of the subscription call; a
    /// refusal is `EX_SECURITY`, which a [`NamedSubscriber`](crate::NamedSubscriber) does not
    /// retry on the same publisher. A subscriber in the publisher's process is seen with the
    /// process's own uid and pid.
    pub authorize: Option<Authorize>,
    /// The pool of slots values are lent from. Default `None`: every value goes through the
    /// ring.
    pub pool: Option<PoolPolicy>,
    /// Whether a publish waits for its subscribers to read what it would overwrite, and a loan
    /// for a slot they have read past, and for how long. Default `None`: neither waits, and a
    /// subscriber that falls more than a ring or [`depth`](PoolPolicy::depth) loans behind loses
    /// the oldest values ([`RecvError::Lagged`](crate::RecvError::Lagged)).
    ///
    /// The wait holds the topic's lock and sleeps the calling thread; [`Backpressure`] says what
    /// waits with it, and what it asks of the binder thread pools.
    pub backpressure: Option<Backpressure>,
}

impl Default for PublisherPolicy {
    fn default() -> Self {
        Self {
            ring_bytes: 64 << 10,
            max_subscribers: 64,
            max_per_uid: 64,
            authorize: None,
            pool: None,
            backpressure: None,
        }
    }
}

/// The pool of a topic that lends slots: [`PublisherPolicy::pool`] (plan 15 §6).
///
/// A loaned value is written straight into a slot of a second memfd that every subscriber maps
/// read-only, and the ring carries only a record of 24 bytes that names the slot. A subscriber
/// pins the slot while it reads it, and the publisher never lends a pinned slot, so a value
/// larger than a cache, which costs its size twice to copy through the ring, is written once.
///
/// The pool has `max_subscribers + 1 + depth + max_loans` slots of `slot_bytes` each: every
/// subscriber pinning one still leaves a slot to lend, with `depth` values kept for subscribers
/// that have not pinned them yet. All of its pages are written when the topic is made and stay
/// the publisher's memory: the default `max_subscribers` of 64 with the default 1 MiB slots is
/// 68 MiB of the publisher's resident memory, whether or not anyone subscribes. Lower
/// `max_subscribers` for a pool of large slots. A pool larger than 4 GiB is refused.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PoolPolicy {
    /// Bytes of a slot, the largest value a loan holds, rounded up to a multiple of 64. Default
    /// 1 MiB.
    pub slot_bytes: usize,
    /// The newest values published that stay readable for a subscriber behind. Default 2; 0
    /// counts as 1.
    ///
    /// Each publish takes back the values older than these, so a subscriber that falls more than
    /// `depth` values behind loses the older ones, as one that falls behind the ring does, and
    /// receives no value older than `depth` publishes. A value a subscriber holds as a
    /// [`Sample`](crate::Sample) stays its own however old it grows. The slot lent next is one
    /// that every subscriber has read past, the one published last of those, so that a busy pool
    /// reuses the slots still in the cache; with none, the oldest.
    ///
    /// With [`PublisherPolicy::backpressure`] no value is taken back: a loan waits instead for a
    /// slot every subscriber has read past, and a subscriber can fall at least `depth + 1` values
    /// behind before a loan waits for it.
    pub depth: usize,
    /// Loans held at once and not yet published, past which a loan is refused. Default 1.
    pub max_loans: usize,
    /// Ask the kernel for transparent huge pages for the pool (`MADV_HUGEPAGE`). Default
    /// `false`.
    ///
    /// Copying out of a slot then takes fewer TLB misses: on an x86_64 desktop with 4 KiB pages
    /// otherwise, a 1 MiB value copied about 6–10 µs faster (plan 15 §8.4). The pool's resident
    /// memory then grows in steps of 2 MiB. Where the system turns huge pages for shared memory
    /// off (`/sys/kernel/mm/transparent_hugepage/shmem_enabled` is `never` or `deny`), it changes
    /// nothing.
    pub huge_pages: bool,
}

impl Default for PoolPolicy {
    fn default() -> Self {
        Self {
            slot_bytes: 1 << 20,
            depth: 2,
            max_loans: 1,
            huge_pages: false,
        }
    }
}

impl PoolPolicy {
    /// Slots and the bytes of each for a topic of `max_subscribers`; `None` past what a pool holds.
    pub(crate) fn shape(&self, max_subscribers: usize) -> Option<(u32, u32)> {
        let grain = layout::SLOT_GRAIN as usize;
        let slot_bytes = self.slot_bytes.max(1).checked_next_multiple_of(grain)?;
        let slots = max_subscribers
            .checked_mul(MAX_PINS as usize)?
            .checked_add(1)?
            .checked_add(self.depth)?
            .checked_add(self.max_loans)?;
        let (slots, slot_bytes) = (u32::try_from(slots).ok()?, u32::try_from(slot_bytes).ok()?);
        layout::pool_size(slots, slot_bytes)?;
        Some((slots, slot_bytes))
    }
}

/// A check of the caller that a topic runs before it takes a subscription:
/// [`PublisherPolicy::authorize`].
///
/// ```
/// use rsbinder_topic::{Authorize, PublisherPolicy};
///
/// let policy = PublisherPolicy {
///     authorize: Some(Authorize::new(|caller| caller.uid == 1000)),
///     ..PublisherPolicy::default()
/// };
/// # drop(policy);
/// ```
///
/// Two values are equal when they are clones of one check.
#[derive(Clone)]
pub struct Authorize(Arc<dyn Fn(&CallingContext) -> bool + Send + Sync>);

impl Authorize {
    /// A check that admits the callers for which `admit` returns `true`. It runs on the binder
    /// thread the subscription call arrived on, while the call waits.
    pub fn new(admit: impl Fn(&CallingContext) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(admit))
    }

    /// Whether the check admits `caller`.
    pub fn admits(&self, caller: &CallingContext) -> bool {
        (self.0)(caller)
    }
}

impl PartialEq for Authorize {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Authorize {}

impl fmt::Debug for Authorize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorize").finish_non_exhaustive()
    }
}

impl PublisherPolicy {
    /// The default policy with a ring that keeps the newest `depth` records of up to `max_payload`
    /// bytes each, including while the next one is being written: `(depth + 1) × (8 +
    /// max_payload rounded up to 8)` bytes, and no less than 4 KiB. A `depth` of zero is taken as
    /// one.
    ///
    /// Smaller records take less of the ring, so more of them are kept. A size above 1 GiB is
    /// refused when the topic is made.
    pub fn for_payload(max_payload: usize, depth: usize) -> Self {
        let record = max_payload.div_ceil(8).saturating_mul(8).saturating_add(8);
        let records = depth.max(1).saturating_add(1);
        Self {
            ring_bytes: records.saturating_mul(record).max(MIN_RING),
            ..Self::default()
        }
    }
}

/// Why a publish wrote nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// The value encodes to more bytes than a record of this ring carries.
    TooLarge {
        /// The encoded value's length.
        bytes: usize,
        /// The longest payload a record of this ring carries: half the ring, less 8.
        max: usize,
    },
    /// [`Payload::encode`] failed with this. A parcelable that holds a binder fails with
    /// `BadType`, one that holds a file descriptor with `FdsNotAllowed`.
    Encode(StatusCode),
    /// The topic has been closed.
    Closed,
    /// A loan from a topic made without a pool ([`PublisherPolicy::pool`] is `None`).
    NoPool,
    /// As many loans as [`PoolPolicy::max_loans`] are held and not yet published or dropped.
    TooManyLoans {
        /// The policy's bound.
        max: usize,
    },
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => write!(
                f,
                "a value of {bytes} bytes is larger than the {max} a record of this topic carries"
            ),
            Self::Encode(code) => write!(f, "the value failed to encode: {code}"),
            Self::Closed => f.write_str("the topic is closed"),
            Self::NoPool => f.write_str("the topic has no pool to lend from"),
            Self::TooManyLoans { max } => {
                write!(
                    f,
                    "{max} loans are held, as many as the topic's pool allows"
                )
            }
        }
    }
}

impl std::error::Error for PublishError {}

impl From<Refused> for PublishError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::TooLarge { bytes, max } => Self::TooLarge { bytes, max },
            Refused::Ended | Refused::Spent => Self::Closed,
        }
    }
}

/// What a publish encodes into, kept for the next one: the allocations of an [`Encoding`].
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    pieces: Vec<Piece<'static>>,
    /// Per record, where its pieces end and its length.
    records: Vec<(usize, usize)>,
}

/// The pool, and the publisher's account of what its slots hold.
pub(crate) struct Lending {
    pub(crate) memory: StoredPool,
    pub(crate) slots: Slots,
    max_loans: usize,
}

/// The pool memory a reply hands out: a duplicate of `fd`, and its shape.
pub(crate) struct PoolFd {
    pub(crate) fd: OwnedFd,
    pub(crate) slots: u32,
    pub(crate) slot_bytes: u32,
}

/// What the publisher's lock guards: the ring's writer and the subscriptions it serves.
pub(crate) struct State<S> {
    pub(crate) writer: Writer<S>,
    pub(crate) links: Links,
    pub(crate) lending: Option<Lending>,
    /// [`PublisherPolicy::backpressure`]: the wait for room; `window`, how far ahead a record goes.
    backpressure: Option<Duration>,
    window: Option<NonZeroUsize>,
    /// Subscriptions a wait took out, told after [`TypedWriter::unlock`] lets go; `evicted` counts.
    evictions: Vec<Eviction>,
    evicted: u64,
    /// Records published, and their payload bytes, for [`PublisherStats`].
    published: u64,
    bytes: u64,
    /// A test's pause after a commit bumps `seq`, before it loads the subscribers' `state`.
    #[cfg(all(test, not(loom)))]
    pub(crate) after_bump: Option<Box<dyn FnMut() + Send>>,
}

impl<S: Stores> State<S> {
    /// After a commit: bump `seq`, then wake the waiters, one futex wake at most (§5.5, §5.5a).
    fn announce(&mut self) {
        let seq = self.writer.announce();
        #[cfg(all(test, not(loom)))]
        if let Some(pause) = self.after_bump.as_mut() {
            pause();
        }
        // Against the `SeqCst` raise of `WAITING`: this load sees it, or the waiter sees new `seq`.
        fence(Ordering::SeqCst);
        if self.links.wake(seq) {
            self.writer.wake();
        }
    }

    /// Wait for room for `len` bytes; unannounced records go out first, or sleepers never read on.
    fn make_room(&mut self, len: usize, unannounced: bool) -> Result<(), Refused> {
        let Some(timeout) = self.backpressure else {
            return Ok(());
        };
        let capacity = self.writer.capacity();
        let mut need = self.writer.end_of(len)?.saturating_sub(capacity);
        if let Some(window) = self.window {
            need = need.max(self.writer.window_floor(window));
        }
        if self.links.lowest_read() >= need {
            return Ok(());
        }
        if unannounced {
            self.announce();
        }
        if wait_for(&self.links, need, Instant::now().checked_add(timeout)) == Room::TimedOut {
            // Every subscription still waited for has read past `need` after this.
            let out = self.links.evict_behind(need);
            self.evicted += out.len() as u64;
            self.evictions.extend(out);
        }
        Ok(())
    }

    /// Lend a slot for `len` bytes, with backpressure one clear of reads and window (plan 15 §6.5).
    fn lend_slot(&mut self, len: usize) -> Result<LentSlot, LendError> {
        let Self {
            writer,
            links,
            lending,
            backpressure,
            window,
            evictions,
            evicted,
            ..
        } = self;
        let lending = lending.as_mut().expect("a lend asks a topic with a pool");
        // Under this lock nothing is written, so the floor holds for the whole wait.
        let floor = window.map_or(0, |window| writer.window_floor(window));
        let mut lend = |links: &Links| {
            let read = links.lowest_read();
            if read < floor {
                return Err(LendError::NoSlot);
            }
            lending.memory.lend(&mut lending.slots, links, read, len)
        };
        let first = lend(links);
        let (Err(LendError::NoSlot), Some(timeout)) = (&first, *backpressure) else {
            return first;
        };
        let mut lent = None;
        let deadline = Instant::now().checked_add(timeout);
        // Only `NoSlot` fails a retry: the loans held change under this lock alone.
        let made = || {
            lent = lend(links).ok();
            lent.is_some()
        };
        wait_until(&*links, made, deadline);
        if let Some(slot) = lent {
            return Ok(slot);
        }
        loop {
            // With none left, every slot not lent or pinned is one to lend.
            let Some(out) = links.evict_lowest() else {
                return lend(links);
            };
            *evicted += 1;
            evictions.push(out);
            match lend(links) {
                Err(LendError::NoSlot) => {}
                other => return other,
            }
        }
    }
}

/// A topic's writer behind a lock, fed with values of `T`. Any thread may publish.
pub(crate) struct TypedWriter<T, S> {
    state: Mutex<State<S>>,
    /// Buffers to encode into, one per thread that publishes at a time.
    scratch: Mutex<Vec<Scratch>>,
    max_payload: usize,
    /// The ring's bytes, the most a buffer keeps when it goes back.
    capacity: usize,
    _payload: PhantomData<fn(&T)>,
}

impl<T: Payload> TypedWriter<T, Stored<Mapping>> {
    /// Make topic memory for `policy`, and its pool if it has one: the fds to hand to
    /// subscribers, and the writer. `BadValue` for a pool past 4 GiB.
    pub(crate) fn create(
        policy: &PublisherPolicy,
    ) -> rsbinder::fmq::Result<(OwnedFd, Option<PoolFd>, Self)> {
        let (fd, stored) = layout::create_topic(policy.ring_bytes)?;
        let writer = Self::new(Writer::new(stored));
        {
            let mut state = writer.lock();
            state.backpressure = policy.backpressure.as_ref().map(|wait| wait.timeout);
            state.window = policy.backpressure.as_ref().and_then(|wait| wait.window);
        }
        let Some(pool) = &policy.pool else {
            return Ok((fd, None, writer));
        };
        let (slots, slot_bytes) = pool
            .shape(policy.max_subscribers)
            .ok_or(rsbinder::fmq::Error::BadValue("pool larger than 4 GiB"))?;
        let (pool_fd, memory) = layout::create_pool(slots, slot_bytes, pool.huge_pages)?;
        let lossless = policy.backpressure.is_some();
        writer.lock().lending = Some(Lending {
            memory,
            slots: Slots::new(slots, pool.depth, pool.max_loans, lossless),
            max_loans: pool.max_loans,
        });
        let pool_fd = PoolFd {
            fd: pool_fd,
            slots,
            slot_bytes,
        };
        Ok((fd, Some(pool_fd), writer))
    }
}

impl<T: Payload, S: Stores> TypedWriter<T, S> {
    pub(crate) fn new(writer: Writer<S>) -> Self {
        Self {
            max_payload: writer.max_payload(),
            capacity: usize::try_from(writer.capacity()).unwrap_or(usize::MAX),
            state: Mutex::new(State {
                writer,
                links: Links::default(),
                lending: None,
                backpressure: None,
                window: None,
                evictions: Vec::new(),
                evicted: 0,
                published: 0,
                bytes: 0,
                #[cfg(all(test, not(loom)))]
                after_bump: None,
            }),
            scratch: Mutex::new(Vec::new()),
            _payload: PhantomData,
        }
    }

    /// What has been published, and the subscriptions held after taking out those that ended.
    pub(crate) fn stats(&self) -> PublisherStats {
        let mut state = self.lock();
        let gone = state.links.prune();
        let stats = PublisherStats {
            published: state.published,
            bytes: state.bytes,
            subscribers: state.links.infos(state.writer.position()),
            evicted: state.evicted,
        };
        drop(state);
        drop(gone);
        stats
    }

    /// The lock every change to the ring and to the list of subscriptions takes.
    pub(crate) fn lock(&self) -> MutexGuard<'_, State<S>> {
        self.state.lock().expect("lock poisoned")
    }

    /// Unlock, then tell evictions and drop `gone`: a call under the lock may run a death notice.
    fn unlock(&self, mut state: MutexGuard<'_, State<S>>, gone: Vec<Arc<Link>>) {
        let evictions = std::mem::take(&mut state.evictions);
        let waited = state.backpressure.unwrap_or_default();
        drop(state);
        for eviction in evictions {
            eviction.tell(T::type_name(), waited);
        }
        drop(gone);
    }

    /// The futex wakes this topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn wakes(&self) -> u64 {
        self.lock().writer.wakes()
    }

    /// The wake bytes this topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn sends(&self) -> u64 {
        self.lock().links.sends()
    }

    /// Write `value` as one record.
    pub(crate) fn publish(&self, value: &T) -> Result<(), PublishError> {
        self.publish_all([value]).map(drop)
    }

    /// Write each value as a record, all of them or, on any error, none; the count written.
    ///
    /// Every value is encoded before the lock is taken: no code of the application runs under it,
    /// so a panic in `encode` leaves the lock as it was, and an `encode` that publishes to this
    /// topic does not wait for itself.
    pub(crate) fn publish_all<'a, I>(&self, values: I) -> Result<usize, PublishError>
    where
        T: 'a,
        I: IntoIterator<Item = &'a T>,
    {
        let mut scratch = self
            .scratch
            .lock()
            .expect("lock poisoned")
            .pop()
            .unwrap_or_default();
        let mut records = std::mem::take(&mut scratch.records);
        records.clear();
        let mut encoding = Encoding::new(
            std::mem::take(&mut scratch.bytes),
            std::mem::take(&mut scratch.pieces),
        );
        let written = self
            .encode(values, &mut encoding, &mut records)
            .and_then(|()| self.write(&encoding, &records));
        let keeps = |len: usize, most: usize| len <= most;
        if keeps(encoding.bytes.capacity(), self.capacity)
            && keeps(encoding.pieces.capacity(), self.capacity / 8)
            && keeps(records.capacity(), self.capacity / 8)
        {
            scratch.bytes = encoding.bytes;
            scratch.pieces = relabel(encoding.pieces);
            scratch.records = records;
            self.scratch.lock().expect("lock poisoned").push(scratch);
        }
        written
    }

    /// Lend a slot of the pool for a value of `len` bytes (plan 15 §6).
    pub(crate) fn lend(&self, len: usize) -> Result<LentSlot, PublishError> {
        let mut state = self.lock();
        let gone = state.links.prune();
        let lent = match &state.lending {
            _ if state.writer.ended() => Err(PublishError::Closed),
            None => Err(PublishError::NoPool),
            Some(lending) if len > lending.memory.slot_bytes() as usize => {
                let max = lending.memory.slot_bytes() as usize;
                Err(PublishError::TooLarge { bytes: len, max })
            }
            Some(lending) => Ok(lending.max_loans),
        };
        let lent = lent.and_then(|max| {
            state.lend_slot(len).map_err(|refused| match refused {
                LendError::TooMany => PublishError::TooManyLoans { max },
                // Pins hold one slot per subscription, at most `max_subscribers`.
                LendError::NoSlot => unreachable!("a pool has a slot past every pin and loan"),
            })
        });
        self.unlock(state, gone);
        lent
    }

    /// Publish the value written into `slot`: its `LOAN` record, then the announcement.
    pub(crate) fn publish_loan(&self, slot: LentSlot) -> Result<(), PublishError> {
        let mut state = self.lock();
        let len = slot.bytes().len();
        let number = slot.slot();
        drop(slot);
        if let Err(refused) = state.make_room(Loan::BYTES, false) {
            if let Some(lending) = state.lending.as_mut() {
                lending.slots.give_back(number);
            }
            return Err(refused.into());
        }
        let State {
            writer, lending, ..
        } = &mut *state;
        let lending = lending.as_mut().expect("a lent slot comes from the pool");
        let end = match writer.end_of(Loan::BYTES) {
            Ok(end) => end,
            Err(refused) => {
                lending.slots.give_back(number);
                return Err(refused.into());
            }
        };
        let generation = lending.slots.publish(&lending.memory, number, end);
        let loan = Loan {
            slot: number,
            // At most `slot_bytes`, a `u32`.
            len: len as u32,
            generation,
        };
        // `end_of` admitted this record, so `put` writes it.
        writer.put(Kind::Loan, &loan.bytes())?;
        state.published += 1;
        state.bytes += len as u64;
        state.announce();
        let gone = state.links.prune();
        self.unlock(state, gone);
        Ok(())
    }

    /// `slot` comes back unpublished.
    pub(crate) fn give_back(&self, slot: LentSlot) {
        let number = slot.slot();
        drop(slot);
        let mut state = self.lock();
        if let Some(lending) = state.lending.as_mut() {
            lending.slots.give_back(number);
        }
    }

    /// Write the end record with `status`. Later publishes are [`PublishError::Closed`]; a second
    /// close writes nothing.
    ///
    /// With backpressure the end record waits for room as a value does, so a subscriber reads
    /// every value before it, and this call (and the publisher's drop) waits up to the timeout.
    pub(crate) fn close(&self, status: &Status) {
        let mut state = self.lock();
        let payload = end::payload(status);
        // A 4 KiB ring takes the longest end record, so only an end already written refuses it.
        if state.make_room(payload.len(), false).is_ok()
            && state.writer.put(Kind::End, &payload).is_ok()
        {
            state.announce();
        }
        self.unlock(state, Vec::new());
    }

    fn encode<'a, I>(
        &self,
        values: I,
        encoding: &mut Encoding<'a>,
        records: &mut Vec<(usize, usize)>,
    ) -> Result<(), PublishError>
    where
        T: 'a,
        I: IntoIterator<Item = &'a T>,
    {
        for value in values {
            encoding.begin();
            value.encode(encoding).map_err(PublishError::Encode)?;
            let bytes = encoding.len;
            if bytes > self.max_payload {
                let max = self.max_payload;
                return Err(PublishError::TooLarge { bytes, max });
            }
            records.push((encoding.pieces.len(), bytes));
        }
        Ok(())
    }

    fn write(
        &self,
        encoding: &Encoding<'_>,
        records: &[(usize, usize)],
    ) -> Result<usize, PublishError> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock();
        // Checked first, so that a refusal comes before the first record and none is written.
        state.writer.admits(records.iter().map(|&(_, len)| len))?;
        let mut first = 0;
        for (written, &(end, len)) in records.iter().enumerate() {
            state.make_room(len, written > 0)?;
            let parts = encoding.pieces[first..end]
                .iter()
                .map(|&piece| encoding.of(piece));
            state.writer.put_parts(Kind::Item, len, parts)?;
            first = end;
        }
        state.published += records.len() as u64;
        state.bytes += records.iter().map(|&(_, len)| len as u64).sum::<u64>();
        state.announce();
        let gone = state.links.prune();
        self.unlock(state, gone);
        Ok(records.len())
    }
}

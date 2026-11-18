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

use rsbinder::{CallingContext, Status, StatusCode};

use crate::backpressure::Lossless;
use crate::backpressure::{wait_for, wait_until, Room};
use crate::layout::{self, Stored, StoredPool, MIN_RING};
use crate::link::{Eviction, Link, Links};
use crate::payload::{relabel, Piece};
use crate::pool::LentSlot;
use crate::pool::{LendError, Loan, Slots, MAX_PINS};
use crate::ring::{Kind, Refused, Stores, Writer};
use crate::{end, Encoding, Payload, PublisherStats, TopicKind};

/// What a topic is made with: the settings of a [`PublisherBuilder`](crate::PublisherBuilder),
/// worked out.
#[derive(Clone, Debug)]
pub(crate) struct Spec {
    pub(crate) kind: TopicKind,
    /// Bytes of ring, 4 KiB to 1 GiB, rounded up to a multiple of 8 when the topic is made.
    pub(crate) ring_bytes: usize,
    pub(crate) max_subscribers: usize,
    pub(crate) max_per_uid: usize,
    pub(crate) authorize: Option<Authorize>,
    pub(crate) pool: Option<PoolSpec>,
    pub(crate) backpressure: Option<Lossless>,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            kind: TopicKind::Event,
            ring_bytes: MIN_RING,
            max_subscribers: 64,
            max_per_uid: 64,
            authorize: None,
            pool: None,
            backpressure: None,
        }
    }
}

/// The pool of a topic that lends slots (plan 15 §6).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PoolSpec {
    /// Bytes of a slot, rounded up to a multiple of 64.
    pub(crate) slot_bytes: usize,
    /// The newest values that stay readable; 0 counts as 1.
    pub(crate) depth: usize,
    /// Loans held at once; 0 counts as 1.
    pub(crate) max_loans: usize,
    pub(crate) huge_pages: bool,
}

impl PoolSpec {
    /// `depth` as the pool keeps it: 0 counts as 1. The slot count and the pool read only this.
    pub(crate) fn kept(&self) -> usize {
        self.depth.max(1)
    }

    /// `max_loans` as the pool lends: 0 counts as 1. The slot count and the pool read only this.
    pub(crate) fn loans(&self) -> usize {
        self.max_loans.max(1)
    }

    /// Slots and the bytes of each for a topic of `max_subscribers`; `None` past what a pool holds.
    pub(crate) fn shape(&self, max_subscribers: usize) -> Option<(u32, u32)> {
        let grain = layout::SLOT_GRAIN as usize;
        let slot_bytes = self.slot_bytes.max(1).checked_next_multiple_of(grain)?;
        let slots = max_subscribers
            .checked_mul(MAX_PINS as usize)?
            .checked_add(1)?
            .checked_add(self.kept())?
            .checked_add(self.loans())?;
        let (slots, slot_bytes) = (u32::try_from(slots).ok()?, u32::try_from(slot_bytes).ok()?);
        layout::pool_size(slots, slot_bytes)?;
        Some((slots, slot_bytes))
    }
}

/// A check of the caller that a topic runs before it takes a subscription:
/// [`PublisherBuilder::authorize`](crate::PublisherBuilder::authorize).
#[derive(Clone)]
pub(crate) struct Authorize(Arc<dyn Fn(&CallingContext) -> bool + Send + Sync>);

impl Authorize {
    pub(crate) fn new(admit: impl Fn(&CallingContext) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(admit))
    }

    pub(crate) fn admits(&self, caller: &CallingContext) -> bool {
        (self.0)(caller)
    }
}

impl fmt::Debug for Authorize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorize").finish_non_exhaustive()
    }
}

/// Bytes of a ring that keeps the newest `depth` records of up to `max_payload` bytes each,
/// including while the next one is being written: `(depth + 1) × (8 + max_payload rounded up to
/// 8)`, and no less than 4 KiB. A `depth` of zero is taken as one. Saturates: a size past 1 GiB
/// is refused when the topic is made.
pub(crate) fn ring_for(max_payload: usize, depth: usize) -> usize {
    let record = max_payload.div_ceil(8).saturating_mul(8).saturating_add(8);
    let records = depth.max(1).saturating_add(1);
    records.saturating_mul(record).max(MIN_RING)
}

/// Why a publish wrote nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
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
    /// A loan from a topic made without a pool
    /// ([`lend`](crate::PublisherBuilder::lend) was not called).
    NoPool,
    /// As many loans as [`max_loans`](crate::PublisherBuilder::max_loans) are held and not yet
    /// published or dropped.
    TooManyLoans {
        /// The topic's bound.
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
    /// [`Lossless`]: the wait for room; `window`, how far ahead a record goes.
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
        // Before the window: no wait or eviction changes the loans held, which only this lock does.
        if lending.slots.at_loan_limit() {
            return Err(LendError::TooMany);
        }
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

#[cfg(all(test, not(loom)))]
thread_local! {
    /// Set by a test: run by a loan's publish after it commits the record, under the lock.
    pub(crate) static AFTER_LOAN_RECORD: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
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

impl<T: Payload> TypedWriter<T, Stored> {
    /// Make topic memory for `spec`, and its pool if it has one: the fds to hand to
    /// subscribers, and the writer. `BadValue` for a pool past 4 GiB.
    pub(crate) fn create(spec: &Spec) -> rsbinder::fmq::Result<(OwnedFd, Option<PoolFd>, Self)> {
        let (fd, stored) = layout::create_topic(spec.ring_bytes)?;
        let writer = Self::new(Writer::new(stored));
        {
            let mut state = writer.lock();
            state.backpressure = spec.backpressure.as_ref().map(|wait| wait.timeout);
            state.window = spec.backpressure.as_ref().and_then(|wait| wait.window);
        }
        let Some(pool) = &spec.pool else {
            return Ok((fd, None, writer));
        };
        let (slots, slot_bytes) = pool
            .shape(spec.max_subscribers)
            .ok_or(rsbinder::fmq::Error::BadValue("pool larger than 4 GiB"))?;
        let (pool_fd, memory) = layout::create_pool(slots, slot_bytes, pool.huge_pages)?;
        let lossless = spec.backpressure.is_some();
        writer.lock().lending = Some(Lending {
            memory,
            slots: Slots::new(slots, pool.kept(), pool.loans(), lossless),
            max_loans: pool.loans(),
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
            advertised: false,
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
        #[cfg(all(test, not(loom)))]
        AFTER_LOAN_RECORD.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
        // After the commit: a value is taken back only once a newer one is readable.
        lending.slots.retire(&lending.memory);
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

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The subscriber's typed side: records decoded into values, and what else a receive returns.

use std::fmt;
use std::marker::PhantomData;
use std::sync::atomic::AtomicU64;

use rsbinder::{Status, StatusCode};

use crate::pool::{PoolReader, Unpinned};
use crate::ring::{Corrupted, Loads, Reader, Taken};
use crate::{end, Payload, Record, Sample, Trust};

/// Why a receive returned no value.
///
/// `Empty`, `Timeout`, `Cancelled`, `Lagged` and `Decode` leave the subscription as it was;
/// `Closed` ends it, and every later receive returns it again. `Disconnected` is the
/// [`NamedSubscriber`](crate::NamedSubscriber)'s, which goes on with the next publisher.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum RecvError {
    /// No record is unread. The next receive tries again from the same place.
    Empty,
    /// A wait with a bound reached it with no record unread. The next receive tries again from
    /// the same place.
    Timeout,
    /// An [`Interrupter`](crate::Interrupter) ended the wait, or the async runtime could not run
    /// it. The next receive tries again from the same place.
    Cancelled,
    /// The publisher overwrote this many records before they were read. The next receive returns
    /// the first record after them.
    ///
    /// A [`NamedSubscriber`](crate::NamedSubscriber)'s drain of a replaced publisher that ends
    /// lapped also counts here the records it leaves unread, and may count fewer than it left,
    /// never more ([`Disconnected::Replaced`]).
    Lagged(u64),
    /// [`Payload::decode`] failed with this for one record, which is skipped. The next receive
    /// returns the record after it.
    Decode(StatusCode),
    /// The topic has ended: every record before the end has been returned.
    Closed(Closed),
    /// A [`NamedSubscriber`](crate::NamedSubscriber)'s connection to one publisher is over, or
    /// could not be made, and the subscriber waits for the next publisher of the name. Returned
    /// once per event; the next receive goes on with the next connection. A
    /// [`Subscriber`](crate::Subscriber) never returns it.
    Disconnected(Disconnected),
}

/// Why a [`NamedSubscriber`](crate::NamedSubscriber) lost or did not get a connection.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Disconnected {
    /// The topic of the connection ended, as [`Closed`] says. Every record before the end was
    /// returned, but for [`Closed::Corrupted`], which ends the connection at the check that
    /// failed.
    ///
    /// After [`Closed::End`], [`Closed::Status`] or [`Closed::PublisherDied`] the subscriber
    /// forgets that publisher, looks the name up once, and connects to the next binder the name
    /// is given to. After `Corrupted` it keeps the publisher and does not connect again until the
    /// name is registered anew, to the same binder or another: while that publisher holds the
    /// name, a receive waits with nothing more to report.
    Ended(Closed),
    /// The publisher refused the subscription with this status. Refusals that a retry cannot
    /// change (`EX_SECURITY`, `EX_UNSUPPORTED_OPERATION`, `BadType`, `InvalidOperation`,
    /// [`ITopic::ERROR_VERSION`]) wait for another publisher to take the name;
    /// [`ITopic::ERROR_FULL`], the other transaction errors and the other exceptions are retried
    /// on the same publisher, at intervals from 1 to 30 seconds, and reported once. `DeadObject`
    /// and [`ITopic::ERROR_CLOSED`] are not reported: the subscriber waits for the name to be
    /// registered anew, as for a refusal a retry cannot change. A connection whose fd the
    /// subscriber's own epoll instance cannot take is reported each time, and asked for again
    /// from a newer publisher of the name at once, else from the same one at those intervals.
    ///
    /// [`ITopic::ERROR_VERSION`]: crate::aidl::ITopic::ERROR_VERSION
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    /// [`ITopic::ERROR_CLOSED`]: crate::aidl::ITopic::ERROR_CLOSED
    Rejected(Status),
    /// Another publisher took the name. Every record the previous one had committed when this
    /// was seen was returned first or missed, and the records missed are counted in a
    /// [`RecvError::Lagged`] before this, exactly or by a lower bound (below), but for those
    /// before the first record the connection read, which are not counted, as a
    /// [`Subscriber`](crate::Subscriber)'s are not. When the name comes back to the previous
    /// publisher before this is returned, it is not: the connection reads on from where the drain
    /// left it, and what the drain counted stays counted.
    ///
    /// The previous publisher, still publishing, can write over those records while they are
    /// read. A read of its ring that it laps five times in a row gives up, as one of
    /// [`Subscriber::try_recv`](crate::Subscriber::try_recv) does, and the records missed are
    /// counted by the index of the next record read, as for any `Lagged`. A lapped read of the
    /// drain resumes at the oldest record the publisher keeps, not half a ring on as a
    /// `Subscriber`'s does, until, twice in a row, the publisher writes over the record such a
    /// resume reached before the drain has read past it; from then on the drain resumes half a
    /// ring on, as a `Subscriber`'s read does, since that publisher writes over the oldest record
    /// before the drain reaches it. Once is not enough: a resume that counts a loss returns the
    /// `Lagged` first and leaves the drain on the oldest record, which a single commit before the
    /// next receive writes over. A publisher that stopped writing before a lap writes over none of
    /// the records from there on, so the drain then returns every record from the oldest kept to
    /// the end. Where a resume would be the end of them or past it, the read gives up instead.
    /// After a second read of the drain that gives up, also one that finds the publisher's process
    /// dead, the drain ends with one `Lagged` for the records it did not return: exact, from the
    /// index of the record after them, unless the publisher wrote over that record too or had not
    /// committed it, and then one, a lower bound, or none when an earlier drain of this
    /// connection counted that record already. A lower bound counts no more records than were
    /// missed, and may count fewer, so no record committed after this was seen is counted. One
    /// receive of the drain makes at most two reads, so a publisher that keeps lapping it holds
    /// no receive; one that breaks the protocol to hold the drain where it is ends it the same
    /// way after two receives in a row that read nothing new.
    ///
    /// A drain that finds the previous publisher's memory failing a check stops there: the
    /// records after that point are neither returned nor counted, since a corrupt ring gives no
    /// index to count them by, and `Replaced` follows with no `Lagged` and no
    /// [`Closed::Corrupted`] before it.
    ///
    /// [`try_latest`](crate::NamedSubscriber::try_latest) and
    /// [`recv_latest`](crate::NamedSubscriber::recv_latest) skip records without counting them,
    /// as a [`Subscriber`](crate::Subscriber)'s do, and return the newest record each time: a
    /// drain of them may return one record the previous publisher committed after this was seen,
    /// always before `Replaced`.
    Replaced,
}

/// How a topic ended.
#[derive(Clone, Debug, PartialEq)]
#[non_exhaustive]
pub enum Closed {
    /// The publisher ended it without an error.
    End,
    /// The publisher ended it with this status.
    Status(Status),
    /// The publisher's process died. Every record it committed was returned before this.
    PublisherDied,
    /// The publisher's memory failed a check: a length or a counter no publisher that follows the
    /// protocol writes. Nothing more is read from it.
    Corrupted,
    /// The publisher took the subscription out: its topic waits for subscribers
    /// ([`PublisherPolicy::backpressure`](crate::PublisherPolicy::backpressure)), and waited for
    /// this one past its timeout. The records it had not read are not returned, as the publisher
    /// no longer keeps them; those before the notice arrived were.
    Evicted,
    /// A [`NamedSubscriber`](crate::NamedSubscriber)'s resolver died: the process that held its
    /// watch of the name is gone, so no publisher of the name will reach the subscriber again.
    /// Only a resolver that names its host reports it ([`Resolver::host`](crate::Resolver::host)):
    /// a registry does, the service manager does not. Returned once no connection is left: a
    /// connection held when the resolver died goes on until it ends, and the `Disconnected` of
    /// that end comes first. Make the subscriber again with a resolver of the new host (plan 15
    /// D8). A [`Subscriber`](crate::Subscriber) never returns it.
    ResolverDied,
}

impl fmt::Display for RecvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => f.write_str("no record is unread"),
            Self::Timeout => f.write_str("no record arrived within the wait"),
            Self::Cancelled => f.write_str("the wait was cancelled"),
            Self::Lagged(records) => write!(f, "{records} records were overwritten unread"),
            Self::Decode(code) => write!(f, "a record failed to decode: {code}"),
            Self::Closed(closed) => fmt::Display::fmt(closed, f),
            Self::Disconnected(why) => fmt::Display::fmt(why, f),
        }
    }
}

impl fmt::Display for Disconnected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Ended(closed) => write!(f, "disconnected: {closed}"),
            Self::Rejected(status) => write!(f, "the publisher refused the subscription: {status}"),
            Self::Replaced => f.write_str("another publisher took the name"),
        }
    }
}

impl fmt::Display for Closed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::End => f.write_str("the topic has ended"),
            Self::Status(status) => write!(f, "the topic has ended with {status}"),
            Self::PublisherDied => f.write_str("the publisher's process died"),
            Self::Corrupted => f.write_str("the topic's memory is corrupted"),
            Self::Evicted => f.write_str("the publisher took the subscription out"),
            Self::ResolverDied => f.write_str("the resolver of the name died"),
        }
    }
}

impl std::error::Error for RecvError {}

/// A reader of a topic's ring that decodes each record as a `T`, and each loan's value once it
/// is copied out of the pool.
pub(crate) struct TypedReader<T, L> {
    reader: Reader<L>,
    /// The publisher's pool, for a topic that lends slots.
    pool: Option<PoolReader>,
    /// What a loan's value is copied into.
    loaned: Vec<u8>,
    /// Loans whose slot was taken back before this reader pinned it.
    lost_loans: u64,
    /// A loan named what no pool of the publisher's has: nothing more is read.
    corrupted: bool,
    _payload: PhantomData<fn() -> T>,
}

impl<T: Payload, L: Loads> TypedReader<T, L> {
    pub(crate) fn new(reader: Reader<L>, pool: Option<PoolReader>) -> Self {
        Self {
            reader,
            pool,
            loaned: Vec::new(),
            lost_loans: 0,
            corrupted: false,
            _payload: PhantomData,
        }
    }

    /// The next value, without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<T, RecvError> {
        self.typed(false)
    }

    /// The newest value, without waiting; the records before it are skipped and not counted as
    /// lost.
    pub(crate) fn try_latest(&mut self) -> Result<T, RecvError> {
        self.typed(true)
    }

    /// [`try_recv`](Self::try_recv), or [`try_latest`](Self::try_latest) with `latest`, that
    /// copies a loan's value with a plain memory copy.
    ///
    /// # Safety
    ///
    /// [`PoolReader::copy_vouched`]'s: the subscriber is [`Trusted`](crate::Trusted).
    pub(crate) unsafe fn recv_vouched(&mut self, latest: bool) -> Result<T, RecvError> {
        // SAFETY: the caller's word for the publisher.
        unsafe { self.typed_as(latest, true) }
    }

    fn typed(&mut self, latest: bool) -> Result<T, RecvError> {
        // SAFETY: not vouched: the copy is by atomic loads, which ask nothing of the publisher.
        unsafe { self.typed_as(latest, false) }
    }

    /// # Safety: with `vouched`, [`recv_vouched`](Self::recv_vouched)'s.
    unsafe fn typed_as(&mut self, latest: bool, vouched: bool) -> Result<T, RecvError> {
        if self.corrupted {
            return Err(RecvError::Closed(Closed::Corrupted));
        }
        let mut retries = 0;
        loop {
            let taken = if latest {
                self.reader.try_latest()
            } else {
                self.reader.try_recv()
            };
            let loan = match taken {
                Ok(Taken::Item { mut record, .. }) => {
                    return T::decode(&mut record).map_err(RecvError::Decode)
                }
                Ok(Taken::Loan { loan, .. }) => loan,
                // The loan lost last was the newest: it is reported.
                Ok(Taken::Empty) if retries > 0 => return Err(self.lost_loan()),
                other => return Err(not_a_value(other)),
            };
            let copied = match &self.pool {
                // SAFETY: `vouched` is the caller's word for the publisher.
                Some(pool) if vouched => unsafe { pool.copy_vouched(loan, &mut self.loaned) },
                Some(pool) => pool.copy(loan, &mut self.loaned),
                None => Err(NO_POOL),
            };
            if goes_on(latest, &copied, &mut retries) {
                continue;
            }
            self.loaned(copied)?;
            let len = self.loaned.len();
            return T::decode(&mut Record::new(&mut self.loaned, len)).map_err(RecvError::Decode);
        }
    }

    /// The next value (the newest with `latest`) in the reader's buffer or a slot this call pins.
    pub(crate) fn take_held(&mut self, latest: bool) -> Result<Held, RecvError> {
        if self.corrupted {
            return Err(RecvError::Closed(Closed::Corrupted));
        }
        let mut retries = 0;
        loop {
            let taken = if latest {
                self.reader.try_latest()
            } else {
                self.reader.try_recv()
            };
            let (loan, at) = match taken {
                Ok(Taken::Item { record, .. }) => {
                    let len = record.bytes().len();
                    return Ok(Held::Ring { len });
                }
                Ok(Taken::Loan { loan, at, .. }) => (loan, at),
                // The loan lost last was the newest: it is reported.
                Ok(Taken::Empty) if retries > 0 => return Err(self.lost_loan()),
                other => return Err(not_a_value(other)),
            };
            let pinned = match &self.pool {
                Some(pool) => pool.pin(loan),
                None => Err(NO_POOL),
            };
            if goes_on(latest, &pinned, &mut retries) {
                continue;
            }
            self.loaned(pinned)?;
            return Ok(Held::Slot {
                slot: loan.slot,
                len: loan.len as usize,
                at,
            });
        }
    }

    /// The sample of what [`take_held`](Self::take_held) returned last, for a subscriber of `M`;
    /// one of a slot stores this reader's position into `read` when dropped.
    pub(crate) fn sample<'a, M: Trust>(&'a self, held: Held, read: &'a AtomicU64) -> Sample<'a, M> {
        match held {
            Held::Ring { len } => Sample::ring(self.reader.held(len)),
            Held::Slot { slot, len, .. } => {
                let pool = self.pool.as_ref().expect("a pinned slot is the pool's");
                Sample::slot(pool, slot, len, read, self.position())
            }
        }
    }

    /// What a pin of a loan's slot, or a copy of it, reports.
    fn loaned(&mut self, done: Result<(), Unpinned>) -> Result<(), RecvError> {
        match done {
            Ok(()) => Ok(()),
            Err(Unpinned::Lost) => Err(self.lost_loan()),
            Err(Unpinned::Corrupted(_)) => {
                self.corrupted = true;
                Err(RecvError::Closed(Closed::Corrupted))
            }
        }
    }

    /// A loan whose slot was taken back before this reader pinned it, counted.
    fn lost_loan(&mut self) -> RecvError {
        self.lost_loans += 1;
        RecvError::Lagged(1)
    }

    /// Where the next record to return starts.
    pub(crate) fn position(&self) -> u64 {
        self.reader.position()
    }

    /// Records and loans reported as lost so far.
    pub(crate) fn lost(&self) -> u64 {
        self.reader.lost().saturating_add(self.lost_loans)
    }

    /// The publisher's pool, as this reader mapped it.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn pool(&self) -> Option<&PoolReader> {
        self.pool.as_ref()
    }

    /// Whether the publisher has committed past this reader.
    pub(crate) fn pending(&self) -> bool {
        self.reader.pending()
    }

    /// Whether the ring reader's last call gave up short of `write` ([`Reader::gave_up`]).
    pub(crate) fn gave_up(&self) -> bool {
        self.reader.gave_up()
    }

    /// [`Reader::count_lost_before`]; none when the reader has failed a check, after which nothing
    /// more is read from it.
    pub(crate) fn count_lost_before(&mut self) -> u64 {
        if self.corrupted {
            return 0;
        }
        self.reader.count_lost_before().unwrap_or(0)
    }

    /// Where the publisher's committed records end now.
    pub(crate) fn committed(&self) -> u64 {
        self.reader.committed()
    }

    /// [`Reader::stop_at`].
    pub(crate) fn stop_at(&mut self, end: Option<u64>) {
        self.reader.stop_at(end);
    }
}

/// Where a value taken by [`TypedReader::take_held`] lies.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Held {
    /// The ring reader's buffer, `len` bytes of it.
    Ring { len: usize },
    /// A slot of the pool, pinned, named by the record that starts at `at`.
    Slot { slot: u32, len: usize, at: u64 },
}

const NO_POOL: Unpinned = Unpinned::Corrupted("a loan from a publisher without a pool");

/// How many loans taken back a receive of the newest value skips before it reports one lost.
pub(crate) const LATEST_RETRIES: u32 = 4;

/// Whether a receive of the newest value skips a loan `done` found taken back, counting the retry.
fn goes_on(latest: bool, done: &Result<(), Unpinned>, retries: &mut u32) -> bool {
    // Taken back only after a newer record was committed (`pool.rs`): skipped as an older one.
    let on = latest && *done == Err(Unpinned::Lost) && *retries < LATEST_RETRIES;
    *retries += u32::from(on);
    on
}

/// What a receive reports for a record that holds no value.
fn not_a_value(taken: Result<Taken<'_>, Corrupted>) -> RecvError {
    match taken {
        Ok(Taken::End { payload }) => RecvError::Closed(match end::status(payload) {
            Some(status) if status.is_ok() => Closed::End,
            Some(status) => Closed::Status(status),
            None => Closed::Corrupted,
        }),
        Ok(Taken::Empty) => RecvError::Empty,
        Ok(Taken::Lagged(records)) => RecvError::Lagged(records),
        // The callers take items and loans before they get here.
        Ok(Taken::Item { .. } | Taken::Loan { .. }) | Err(Corrupted(_)) => {
            RecvError::Closed(Closed::Corrupted)
        }
    }
}

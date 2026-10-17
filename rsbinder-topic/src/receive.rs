// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The subscriber's typed side: records decoded into values, and what else a receive returns.

use std::fmt;
use std::marker::PhantomData;

use rsbinder::{Status, StatusCode};

use crate::pool::{PoolReader, Unpinned};
use crate::ring::{Corrupted, Loads, Reader, Taken};
use crate::{end, Payload, Record, Sample};

/// Why a receive returned no value.
///
/// `Empty`, `Timeout`, `Cancelled`, `Lagged` and `Decode` leave the subscription as it was;
/// `Closed` ends it, and every later receive returns it again. `Disconnected` is the
/// [`NamedSubscriber`](crate::NamedSubscriber)'s, which goes on with the next publisher.
#[derive(Clone, Debug, PartialEq)]
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
pub enum Disconnected {
    /// The topic of the connection ended, as [`Closed`] says. Every record before the end was
    /// returned.
    Ended(Closed),
    /// The publisher refused the subscription with this status. Refusals that a retry cannot
    /// change (`EX_SECURITY`, `BadType`, [`ITopic::ERROR_VERSION`]) wait for another publisher
    /// to take the name; [`ITopic::ERROR_FULL`] and transaction errors are retried on the same
    /// publisher, at intervals from 1 to 30 seconds, and reported once.
    ///
    /// [`ITopic::ERROR_VERSION`]: crate::aidl::ITopic::ERROR_VERSION
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    Rejected(Status),
    /// Another publisher took the name. Every record the previous one had committed when this
    /// was seen was returned first.
    Replaced,
}

/// How a topic ended.
#[derive(Clone, Debug, PartialEq)]
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

    fn typed(&mut self, latest: bool) -> Result<T, RecvError> {
        if self.corrupted {
            return Err(RecvError::Closed(Closed::Corrupted));
        }
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
            other => return Err(not_a_value(other)),
        };
        let copied = match &self.pool {
            Some(pool) => pool.copy(loan, &mut self.loaned),
            None => Err(NO_POOL),
        };
        self.loaned(copied)?;
        let len = self.loaned.len();
        T::decode(&mut Record::new(&mut self.loaned, len)).map_err(RecvError::Decode)
    }

    /// The next value where it lies, the ring's buffer or a slot this call pins, without waiting.
    pub(crate) fn take_held(&mut self) -> Result<Held, RecvError> {
        if self.corrupted {
            return Err(RecvError::Closed(Closed::Corrupted));
        }
        let loan = match self.reader.try_recv() {
            Ok(Taken::Item { record, .. }) => {
                let len = record.bytes().len();
                return Ok(Held::Ring { len });
            }
            Ok(Taken::Loan { loan, .. }) => loan,
            other => return Err(not_a_value(other)),
        };
        let pinned = match &self.pool {
            Some(pool) => pool.pin(loan),
            None => Err(NO_POOL),
        };
        self.loaned(pinned)?;
        Ok(Held::Slot {
            slot: loan.slot,
            len: loan.len as usize,
        })
    }

    /// The sample of what [`take_held`](Self::take_held) returned last.
    pub(crate) fn sample(&self, held: Held) -> Sample<'_> {
        match held {
            Held::Ring { len } => Sample::ring(self.reader.held(len)),
            Held::Slot { slot, len } => {
                let pool = self.pool.as_ref().expect("a pinned slot is the pool's");
                Sample::slot(pool, slot, len)
            }
        }
    }

    /// What a pin of a loan's slot, or a copy of it, reports.
    fn loaned(&mut self, done: Result<(), Unpinned>) -> Result<(), RecvError> {
        match done {
            Ok(()) => Ok(()),
            Err(Unpinned::Lost) => {
                self.lost_loans += 1;
                Err(RecvError::Lagged(1))
            }
            Err(Unpinned::Corrupted(_)) => {
                self.corrupted = true;
                Err(RecvError::Closed(Closed::Corrupted))
            }
        }
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

    /// Where the publisher's committed records end now.
    pub(crate) fn committed(&self) -> u64 {
        self.reader.committed()
    }
}

/// Where a value taken by [`TypedReader::take_held`] lies.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Held {
    /// The ring reader's buffer, `len` bytes of it.
    Ring { len: usize },
    /// A slot of the pool, pinned.
    Slot { slot: u32, len: usize },
}

const NO_POOL: Unpinned = Unpinned::Corrupted("a loan from a publisher without a pool");

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

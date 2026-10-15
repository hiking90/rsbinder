// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The subscriber's typed side: records decoded into values, and what else a receive returns.

use std::fmt;
use std::marker::PhantomData;

use rsbinder::{Status, StatusCode};

use crate::ring::{Corrupted, Loads, Reader, Taken};
use crate::{end, Payload};

/// Why a receive returned no value.
///
/// `Empty`, `Lagged` and `Decode` leave the subscription as it was; `Closed` ends it, and every
/// later receive returns it again.
#[derive(Clone, Debug, PartialEq)]
pub enum RecvError {
    /// No record is unread. The next receive tries again from the same place.
    Empty,
    /// The publisher overwrote this many records before they were read. The next receive returns
    /// the first record after them.
    Lagged(u64),
    /// [`Payload::decode`] failed with this for one record, which is skipped. The next receive
    /// returns the record after it.
    Decode(StatusCode),
    /// The topic has ended: every record before the end has been returned.
    Closed(Closed),
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
            Self::Lagged(records) => write!(f, "{records} records were overwritten unread"),
            Self::Decode(code) => write!(f, "a record failed to decode: {code}"),
            Self::Closed(closed) => fmt::Display::fmt(closed, f),
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

/// A reader of a topic's ring that decodes each record as a `T`.
pub(crate) struct TypedReader<T, L> {
    reader: Reader<L>,
    _payload: PhantomData<fn() -> T>,
}

impl<T: Payload, L: Loads> TypedReader<T, L> {
    pub(crate) fn new(reader: Reader<L>) -> Self {
        Self {
            reader,
            _payload: PhantomData,
        }
    }

    /// The next value, without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<T, RecvError> {
        typed(self.reader.try_recv())
    }

    /// The newest value, without waiting; the records before it are skipped and not counted as
    /// lost.
    pub(crate) fn try_latest(&mut self) -> Result<T, RecvError> {
        typed(self.reader.try_latest())
    }

    /// Where the next record to return starts.
    pub(crate) fn position(&self) -> u64 {
        self.reader.position()
    }

    /// Records reported as lost so far.
    pub(crate) fn lost(&self) -> u64 {
        self.reader.lost()
    }
}

fn typed<T: Payload>(taken: Result<Taken<'_>, Corrupted>) -> Result<T, RecvError> {
    match taken {
        Ok(Taken::Item { payload, .. }) => T::decode(payload).map_err(RecvError::Decode),
        Ok(Taken::End { payload }) => Err(RecvError::Closed(match end::status(payload) {
            Some(status) if status.is_ok() => Closed::End,
            Some(status) => Closed::Status(status),
            None => Closed::Corrupted,
        })),
        Ok(Taken::Empty) => Err(RecvError::Empty),
        Ok(Taken::Lagged(records)) => Err(RecvError::Lagged(records)),
        Err(Corrupted(_)) => Err(RecvError::Closed(Closed::Corrupted)),
    }
}

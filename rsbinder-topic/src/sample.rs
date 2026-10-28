// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A value received where it lies (plan 15 §6.3).

use std::fmt;
use std::marker::PhantomData;
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::pool::PoolReader;
use crate::{Trust, Trusted, Untrusted};

/// A value of a [`Bytes`](crate::Bytes) topic, received where it lies:
/// [`Subscriber::recv_sample`](crate::Subscriber::recv_sample) and the calls beside it.
///
/// A value the publisher lent ([`Publisher::loan_slice`](crate::Publisher::loan_slice)) stays in
/// its slot of the topic's pool, which the sample pins: the publisher lends no pinned slot, and
/// dropping the sample lets go. The subscriber reports a lent value read only then, so a
/// publisher with [`Backpressure`](crate::Backpressure) that waits for it waits for the drop. A
/// value that came through the ring is in the subscriber's buffer. Either way the sample borrows
/// the subscriber, so it holds one at a time.
///
/// How a lent value is read depends on whether the subscriber vouches for its publisher, `M`:
///
/// - [`Untrusted`], the subscriber [`Subscriber::request`](crate::Subscriber::request) makes:
///   [`copy_to`](Self::copy_to) and [`to_owned`](Self::to_owned) copy the value out with atomic
///   loads, so a publisher that does not keep the pin protocol and writes the slot meanwhile
///   makes the copy wrong, and nothing worse. `unsafe` [`as_slice`](Self::as_slice) reads it in
///   place and asks the caller to vouch for the publisher at each call.
/// - [`Trusted`], after [`trust_publisher`](crate::Subscriber::trust_publisher): the sample
///   derefs to `[u8]` in place, and `copy_to` and `to_owned` are a plain memory copy.
///
/// An untrusted sample does not deref:
///
/// ```compile_fail
/// fn first(sample: &rsbinder_topic::Sample<'_>) -> Option<u8> {
///     sample.first().copied()
/// }
/// ```
///
/// A trusted one does:
///
/// ```
/// use rsbinder_topic::{Sample, Trusted};
///
/// fn first(sample: &Sample<'_, Trusted>) -> Option<u8> {
///     sample.first().copied()
/// }
/// ```
pub struct Sample<'a, M: Trust = Untrusted> {
    source: Source<'a>,
    /// A lent value's: the subscriber's `read` word, and where its record ends, stored on drop.
    done: Option<(&'a AtomicU64, u64)>,
    _trust: PhantomData<fn() -> M>,
}

enum Source<'a> {
    Ring(&'a [u8]),
    Slot {
        pool: &'a PoolReader,
        slot: u32,
        len: usize,
    },
}

impl<'a, M: Trust> Sample<'a, M> {
    pub(crate) fn ring(bytes: &'a [u8]) -> Self {
        Self {
            source: Source::Ring(bytes),
            done: None,
            _trust: PhantomData,
        }
    }

    /// A sample of `slot`, which the caller pinned through `pool`. Dropped, it stores `end` into
    /// `read`, the subscriber's word: only then has the subscriber read the record.
    pub(crate) fn slot(
        pool: &'a PoolReader,
        slot: u32,
        len: usize,
        read: &'a AtomicU64,
        end: u64,
    ) -> Self {
        Self {
            source: Source::Slot { pool, slot, len },
            done: Some((read, end)),
            _trust: PhantomData,
        }
    }

    /// Bytes in the value.
    pub fn len(&self) -> usize {
        match self.source {
            Source::Ring(bytes) => bytes.len(),
            Source::Slot { len, .. } => len,
        }
    }

    /// Whether the value has no bytes.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy the value into `dst`: by atomic loads for a lent value of an [`Untrusted`]
    /// subscriber, a plain memory copy otherwise.
    ///
    /// # Panics
    ///
    /// When `dst` is not [`len`](Self::len) bytes long, as `<[u8]>::copy_from_slice` does.
    pub fn copy_to(&self, dst: &mut [u8]) {
        assert_eq!(dst.len(), self.len(), "a destination of another length");
        if let Some(bytes) = self.in_reach() {
            dst.copy_from_slice(bytes);
        } else if let Source::Slot { pool, slot, .. } = self.source {
            pool.pool().load_slot(slot, dst);
        }
    }

    /// The value, copied into a new buffer: filled by atomic loads, for a lent value of an
    /// [`Untrusted`] subscriber; one plain memory copy otherwise.
    pub fn to_owned(&self) -> Vec<u8> {
        if let Some(bytes) = self.in_reach() {
            return bytes.to_vec();
        }
        let mut value = Vec::with_capacity(self.len());
        if let Source::Slot { pool, slot, len } = self.source {
            pool.pool().load_slot_extend(slot, len, &mut value);
        }
        value
    }

    /// The value as a slice where that asks nothing of the caller: in the buffer, or [`Trusted`]'s.
    fn in_reach(&self) -> Option<&[u8]> {
        match self.source {
            Source::Ring(bytes) => Some(bytes),
            Source::Slot { pool, slot, len } if M::TRUSTED => {
                // SAFETY: the pin this sample holds, and the word `trust_publisher` took.
                Some(unsafe { pool.pool().slot_slice(slot, len) })
            }
            Source::Slot { .. } => None,
        }
    }

    /// Whether the value lies in a slot of the pool, for tests of where samples come from.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn is_loaned(&self) -> bool {
        matches!(self.source, Source::Slot { .. })
    }
}

impl Sample<'_, Untrusted> {
    /// The value in place, with no copy.
    ///
    /// The slice borrows the sample mutably, so [`copy_to`](Self::copy_to) and
    /// [`to_owned`](Self::to_owned), which read the slot another way, wait until it is gone. A
    /// subscriber that vouches for its publisher once, rather than at each call, is
    /// [`Trusted`](crate::Trusted), whose samples deref to the slice.
    ///
    /// # Safety
    ///
    /// The topic's publisher keeps the pin protocol of plan 15 §6.2: it never writes a slot a
    /// subscriber has pinned. An `rsbinder-topic` [`Publisher`](crate::Publisher) does. A
    /// publisher that does not can change the bytes while the slice lives, which `&[u8]` rules
    /// out. A process that cannot vouch for its publisher — one less privileged than itself,
    /// say — reads with `copy_to`. A value that came through the ring lies in this process's
    /// own memory, which no publisher writes.
    pub unsafe fn as_slice(&mut self) -> &[u8] {
        match self.source {
            Source::Ring(bytes) => bytes,
            // SAFETY: the pin this sample holds, and the caller's word for the publisher.
            Source::Slot { pool, slot, len } => unsafe { pool.pool().slot_slice(slot, len) },
        }
    }
}

impl Deref for Sample<'_, Trusted> {
    type Target = [u8];

    fn deref(&self) -> &[u8] {
        self.in_reach().expect("a trusted sample is in reach")
    }
}

impl AsRef<[u8]> for Sample<'_, Trusted> {
    fn as_ref(&self) -> &[u8] {
        self
    }
}

impl<M: Trust> Drop for Sample<'_, M> {
    fn drop(&mut self) {
        if let Source::Slot { pool, .. } = self.source {
            pool.unpin();
        }
        if let Some((read, end)) = self.done {
            // As a receive's: a publisher that waits for this `read` counts the value read now.
            read.store(end, Ordering::Release);
        }
    }
}

impl<M: Trust> fmt::Debug for Sample<'_, M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sample")
            .field("len", &self.len())
            .field("trusted", &M::TRUSTED)
            .finish_non_exhaustive()
    }
}

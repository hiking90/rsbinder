// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A value received where it lies (plan 15 §6.3).

use std::fmt;

use crate::pool::PoolReader;

/// A value of a [`Bytes`](crate::Bytes) topic, received where it lies:
/// [`Subscriber::recv_sample`](crate::Subscriber::recv_sample) and the calls beside it.
///
/// A value the publisher lent ([`Publisher::loan_slice`](crate::Publisher::loan_slice)) stays in
/// its slot of the topic's pool, which the sample pins: the publisher lends no pinned slot, and
/// dropping the sample lets go. A value that came through the ring is in the subscriber's buffer.
/// Either way the sample borrows the subscriber, so it holds one at a time.
///
/// [`copy_to`](Self::copy_to) and [`to_owned`](Self::to_owned) copy the value out with atomic
/// loads: a publisher that does not keep the pin protocol and writes the slot meanwhile makes the
/// copy wrong, and nothing worse. [`as_slice`](Self::as_slice) reads it in place with no copy, and
/// asks the caller to vouch for the publisher.
pub struct Sample<'a> {
    source: Source<'a>,
}

enum Source<'a> {
    Ring(&'a [u8]),
    Slot {
        pool: &'a PoolReader,
        slot: u32,
        len: usize,
    },
}

impl<'a> Sample<'a> {
    pub(crate) fn ring(bytes: &'a [u8]) -> Self {
        Self {
            source: Source::Ring(bytes),
        }
    }

    /// A sample of `slot`, which the caller pinned through `pool`.
    pub(crate) fn slot(pool: &'a PoolReader, slot: u32, len: usize) -> Self {
        Self {
            source: Source::Slot { pool, slot, len },
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

    /// Copy the value into `dst`.
    ///
    /// # Panics
    ///
    /// When `dst` is not [`len`](Self::len) bytes long, as `<[u8]>::copy_from_slice` does.
    pub fn copy_to(&self, dst: &mut [u8]) {
        assert_eq!(dst.len(), self.len(), "a destination of another length");
        match self.source {
            Source::Ring(bytes) => dst.copy_from_slice(bytes),
            Source::Slot { pool, slot, .. } => pool.pool().load_slot(slot, dst),
        }
    }

    /// The value, copied into a new buffer.
    pub fn to_owned(&self) -> Vec<u8> {
        let mut value = vec![0; self.len()];
        self.copy_to(&mut value);
        value
    }

    /// The value in place, with no copy.
    ///
    /// The slice borrows the sample mutably, so [`copy_to`](Self::copy_to) and
    /// [`to_owned`](Self::to_owned), which read the slot another way, wait until it is gone.
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

    /// Whether the value lies in a slot of the pool, for tests of where samples come from.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn is_loaned(&self) -> bool {
        matches!(self.source, Source::Slot { .. })
    }
}

impl Drop for Sample<'_> {
    fn drop(&mut self) {
        if let Source::Slot { pool, .. } = self.source {
            pool.unpin();
        }
    }
}

impl fmt::Debug for Sample<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Sample")
            .field("len", &self.len())
            .finish_non_exhaustive()
    }
}

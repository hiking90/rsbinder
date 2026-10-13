// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a reader concludes from the two shared counters, as functions of numbers: `unsync.rs`
//! applies them to mapped memory and `loom_tests.rs` to a loom model of it. libfmq
//! `MessageQueueBase.h`: `processOverflow`, `readLatest`.

use crate::error::{Error, Result};

/// A ring's fixed numbers, in bytes. `bytes` is a multiple of `quantum`; neither is zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Shape {
    pub(super) bytes: u64,
    pub(super) quantum: u64,
}

impl Shape {
    /// What a lapped reader keeps: half the ring, in whole elements (libfmq `wantToSave`).
    fn half(self) -> u64 {
        self.bytes / self.quantum / 2 * self.quantum
    }
}

/// The two counters as one reader loaded them, checked against a hostile or defective writer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct View {
    /// The write counter: bytes committed.
    pub(super) write: u64,
    /// `max(write region end, write)`: bytes committed or being written.
    front: u64,
}

impl View {
    /// `seen` is the highest `write` this reader loaded before; `end` was loaded before `write`.
    pub(super) fn new(seen: u64, write: u64, end: u64, shape: Shape) -> Result<Self> {
        if write % shape.quantum != 0 {
            return Err(Error::Corrupted(
                "write counter not a multiple of the element size",
            ));
        }
        if write < seen {
            return Err(Error::Corrupted("write counter moved backwards"));
        }
        // A reservation adds at most `bytes`; a counter this high would wrap.
        if write > u64::MAX - shape.bytes {
            return Err(Error::Corrupted("counter too close to wrapping"));
        }
        let front = end.max(write);
        // A writer reserves at most the ring, from a `write` no newer than the one loaded here.
        if front - write > shape.bytes {
            return Err(Error::Corrupted("more bytes reserved than the ring holds"));
        }
        Ok(Self { write, front })
    }

    /// Where a reader at `read` resumes when the writer has lapped it, `None` when it has not.
    pub(super) fn lapped(&self, read: u64, shape: Shape) -> Option<u64> {
        if self.front - read <= shape.bytes {
            return None;
        }
        // Half a ring behind the writer, unless the write in progress reaches into that half.
        let reserved = self.front - self.write;
        let keep = if shape.half() <= shape.bytes - reserved {
            shape.half()
        } else {
            0
        };
        Some(self.write - keep)
    }

    /// The newest `want` bytes a reader at `read` may copy: `(start, length)`, whole elements.
    pub(super) fn latest(&self, read: u64, want: u64, shape: Shape) -> (u64, u64) {
        let oldest = read.max(self.front.saturating_sub(shape.bytes));
        let available = (self.write - oldest) / shape.quantum * shape.quantum;
        let length = available.min(want);
        (self.write - length, length)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHAPE: Shape = Shape {
        bytes: 64,
        quantum: 4,
    };

    fn view(write: u64, end: u64) -> View {
        View::new(0, write, end, SHAPE).unwrap()
    }

    fn corrupted(seen: u64, write: u64, end: u64) -> &'static str {
        match View::new(seen, write, end, SHAPE) {
            Err(Error::Corrupted(what)) => what,
            other => panic!("expected Corrupted, got {other:?}"),
        }
    }

    #[test]
    fn a_reader_within_one_ring_of_the_front_is_not_lapped() {
        assert_eq!(view(64, 64).lapped(0, SHAPE), None);
        assert_eq!(view(100, 100).lapped(36, SHAPE), None);
        // A writer older than the region end leaves it at zero: `write` alone decides.
        assert_eq!(view(64, 0).lapped(0, SHAPE), None);
        assert_eq!(view(68, 0).lapped(0, SHAPE), Some(36));
    }

    #[test]
    fn a_reservation_counts_as_written() {
        // Committed bytes fit, but the write in progress reaches the reader's position.
        assert_eq!(view(60, 68).lapped(0, SHAPE), Some(28));
        assert_eq!(view(60, 64).lapped(0, SHAPE), None);
    }

    #[test]
    fn a_lapped_reader_resumes_half_a_ring_behind_the_writer() {
        assert_eq!(view(200, 200).lapped(0, SHAPE), Some(168));
        // 32 bytes are reserved, which leaves exactly the half.
        assert_eq!(view(200, 232).lapped(0, SHAPE), Some(168));
    }

    #[test]
    fn a_reservation_into_the_kept_half_resumes_at_the_writer() {
        assert_eq!(view(200, 236).lapped(0, SHAPE), Some(200));
        assert_eq!(view(200, 264).lapped(100, SHAPE), Some(200));
    }

    #[test]
    fn the_resume_position_is_ahead_of_the_reader_and_a_whole_element() {
        for write in (0..400).step_by(4) {
            for reserved in (0..=64).step_by(4) {
                let view = view(write, write + reserved);
                for read in (0..=write).step_by(4) {
                    if let Some(resume) = view.lapped(read, SHAPE) {
                        assert!(resume > read, "{write} {reserved} {read}");
                        assert!(resume <= write && resume % 4 == 0);
                        assert_eq!(view.lapped(resume, SHAPE), None);
                    }
                }
            }
        }
    }

    #[test]
    fn counters_no_writer_produces_are_corrupted() {
        assert!(corrupted(0, 6, 6).contains("multiple"));
        assert!(corrupted(8, 4, 4).contains("backwards"));
        assert!(corrupted(0, u64::MAX - 63, 0).contains("wrapping"));
        assert!(corrupted(0, 100, 168).contains("reserved"));
        assert!(corrupted(0, 0, u64::MAX).contains("reserved"));
        assert_eq!(view(u64::MAX - 67, 0).write, u64::MAX - 67);
    }

    #[test]
    fn latest_is_the_newest_unread_elements_outside_the_reservation() {
        // Nothing lost: everything unread, capped by what was asked for.
        assert_eq!(view(40, 40).latest(0, 64, SHAPE), (0, 40));
        assert_eq!(view(40, 40).latest(0, 8, SHAPE), (32, 8));
        assert_eq!(view(40, 40).latest(40, 8, SHAPE), (40, 0));
        // Lapped: one ring at most.
        assert_eq!(view(200, 200).latest(0, 400, SHAPE), (136, 64));
        // The 12 bytes being written take the oldest 12 of that ring.
        assert_eq!(view(200, 212).latest(0, 400, SHAPE), (148, 52));
        // A region end that is no whole element still yields whole elements.
        assert_eq!(view(200, 210).latest(0, 400, SHAPE), (148, 52));
        // A reservation of the whole ring, which libfmq's writer may make, leaves nothing.
        assert_eq!(view(200, 264).latest(100, 4, SHAPE), (200, 0));
    }
}

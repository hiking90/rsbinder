// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a reader concludes from the publisher's counters and from a record's header, as functions
//! of numbers; `ring.rs` applies them to memory.
//!
//! The counters are loaded one at a time while the publisher keeps storing them, so a check on two
//! of them is sound only for one order of the two loads. With `a <= b` at every instant and both
//! only ever growing, `a` loaded first gives `a <= b`: the `b` loaded later is at least the `b` of
//! the moment `a` was loaded. With `b - a <= n` it is the other way round, `b` first. Each function
//! names the order it was written for, and none of them fails a publisher that follows the
//! protocol, wherever between its stores the loads fall. `frontier - tail <= capacity` holds too:
//! a record's `tail` is stored before any of its `frontier`s, which reach at most its end.

use super::Corrupted;

/// Bytes of a record's header: a word of kind and length, and the record's index.
pub(super) const HEADER: u64 = 8;

/// The low 30 bits of a header's first word: the payload's length.
const LENGTH: u32 = (1 << 30) - 1;

/// Bytes a record with `len` bytes of payload takes: the header, and the payload padded to 8.
pub(super) fn record_size(len: u32) -> u64 {
    HEADER + u64::from(len).div_ceil(8) * 8
}

/// A record's header as it lies in the ring: two little-endian words.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Header {
    /// The top two bits of the first word.
    pub(super) kind: u32,
    /// The payload's length in bytes, without the padding.
    pub(super) len: u32,
    /// The publisher's count of records before this one, modulo 2^32.
    pub(super) index: u32,
}

impl Header {
    pub(super) fn bytes(self) -> [u8; 8] {
        let mut out = [0; 8];
        out[..4].copy_from_slice(&(self.kind << 30 | self.len & LENGTH).to_le_bytes());
        out[4..].copy_from_slice(&self.index.to_le_bytes());
        out
    }

    pub(super) fn parse(bytes: [u8; 8]) -> Self {
        let [a, b, c, d, e, f, g, h] = bytes;
        let word = u32::from_le_bytes([a, b, c, d]);
        Self {
            kind: word >> 30,
            len: word & LENGTH,
            index: u32::from_le_bytes([e, f, g, h]),
        }
    }

    /// The record's size, if a publisher could have committed it at `position`, below `write`.
    pub(super) fn extent(self, position: u64, write: u64, capacity: u64) -> Result<u64, Corrupted> {
        let size = record_size(self.len);
        if size > capacity / 2 {
            return Err(Corrupted("record longer than half the ring"));
        }
        if size > write.saturating_sub(position) {
            return Err(Corrupted("record reaches past the write counter"));
        }
        Ok(size)
    }
}

/// `tail` and `write` as one reader loaded them, checked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct View {
    /// The start of the oldest record the publisher has not overwritten.
    pub(super) tail: u64,
    /// Bytes committed.
    pub(super) write: u64,
}

/// The highest counters a reader has loaded so far. A publisher's counters never go back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Seen {
    tail: u64,
    write: u64,
    frontier: u64,
}

impl Seen {
    /// The newest `write` loaded. The reader never moves past it.
    pub(super) fn write(&self) -> u64 {
        self.write
    }

    /// Judge three loads made in this order: `tail` (as `before`), `write`, `tail` again.
    pub(super) fn view(
        &mut self,
        before: u64,
        write: u64,
        tail: u64,
        capacity: u64,
    ) -> Result<View, Corrupted> {
        if before % 8 != 0 || tail % 8 != 0 {
            return Err(Corrupted("tail off the record grid"));
        }
        if before < self.tail || tail < before {
            return Err(Corrupted("tail moved backwards"));
        }
        self.newer(write)?;
        // `tail <= write` at every instant; `before` was loaded first.
        if before > write {
            return Err(Corrupted("tail past the write counter"));
        }
        // `write - tail <= capacity` at every instant; `tail` was loaded last.
        if write.saturating_sub(tail) > capacity {
            return Err(Corrupted("more bytes kept than the ring holds"));
        }
        self.tail = tail;
        Ok(View { tail, write })
    }

    /// A `write` loaded after every earlier one.
    pub(super) fn newer(&mut self, write: u64) -> Result<(), Corrupted> {
        if write % 8 != 0 {
            return Err(Corrupted("write counter off the record grid"));
        }
        if write < self.write {
            return Err(Corrupted("write counter moved backwards"));
        }
        self.write = write;
        Ok(())
    }

    /// A `tail` loaded after every earlier load, of `write` too.
    pub(super) fn tail(&mut self, tail: u64, capacity: u64) -> Result<u64, Corrupted> {
        if tail % 8 != 0 {
            return Err(Corrupted("tail off the record grid"));
        }
        if tail < self.tail {
            return Err(Corrupted("tail moved backwards"));
        }
        // `write - tail <= capacity` at every instant, and the `write` held was loaded earlier.
        if self.write.saturating_sub(tail) > capacity {
            return Err(Corrupted("more bytes kept than the ring holds"));
        }
        self.tail = tail;
        Ok(tail)
    }

    /// [`tail`](Self::tail), after the `frontier` that found a lap: a ring behind it at most.
    pub(super) fn lapped_tail(&mut self, tail: u64, capacity: u64) -> Result<u64, Corrupted> {
        let tail = self.tail(tail, capacity)?;
        // `frontier - tail <= capacity` at every instant, and the `frontier` held was loaded first.
        if self.frontier.saturating_sub(tail) > capacity {
            return Err(Corrupted("frontier more than a ring past tail"));
        }
        Ok(tail)
    }

    /// A `frontier` loaded after every earlier load, of `write` too.
    pub(super) fn frontier(&mut self, frontier: u64) -> Result<u64, Corrupted> {
        if frontier % 8 != 0 {
            return Err(Corrupted("frontier off the record grid"));
        }
        if frontier < self.frontier {
            return Err(Corrupted("frontier moved backwards"));
        }
        // Stored up to a record's end before its `write`, and the `write` held was loaded earlier.
        if frontier < self.write {
            return Err(Corrupted("frontier behind the write counter"));
        }
        self.frontier = frontier;
        Ok(frontier)
    }

    /// A record start kept in a word (`tail`, `half`, `last`): from `tail` on, before `write`.
    pub(super) fn kept(
        &mut self,
        start: u64,
        write: u64,
        what: &'static str,
    ) -> Result<u64, Corrupted> {
        self.newer(write)?;
        // `tail <= write` at every instant, and the `tail` held was loaded before this `write`.
        if self.tail > write {
            return Err(Corrupted("tail past the write counter"));
        }
        // `tail <= half <= last < write` at every instant once a record is committed.
        if start % 8 != 0 || start < self.tail || start >= write {
            return Err(Corrupted(what));
        }
        Ok(start)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CAPACITY: u64 = 64;

    fn what<T: std::fmt::Debug>(result: Result<T, Corrupted>) -> &'static str {
        result.expect_err("a corrupted view").0
    }

    #[test]
    fn a_record_is_its_header_and_its_payload_padded_to_eight() {
        assert_eq!(record_size(0), 8);
        assert_eq!(record_size(1), 16);
        assert_eq!(record_size(8), 16);
        assert_eq!(record_size(9), 24);
        assert_eq!(record_size(LENGTH), (1 << 30) + 8);
    }

    #[test]
    fn a_header_round_trips_and_lies_little_endian() {
        let header = Header {
            kind: 1,
            len: 0x0123_4567,
            index: 0x89ab_cdef,
        };
        let bytes = header.bytes();
        assert_eq!(bytes, [0x67, 0x45, 0x23, 0x41, 0xef, 0xcd, 0xab, 0x89]);
        assert_eq!(Header::parse(bytes), header);
        for kind in 0..4 {
            let header = Header {
                kind,
                len: LENGTH,
                index: u32::MAX,
            };
            assert_eq!(Header::parse(header.bytes()), header);
        }
    }

    #[test]
    fn a_record_must_fit_half_the_ring_and_the_committed_bytes() {
        let header = |len| Header {
            kind: 0,
            len,
            index: 0,
        };
        assert_eq!(header(24).extent(0, 32, CAPACITY), Ok(32));
        assert_eq!(header(0).extent(8, 16, CAPACITY), Ok(8));
        assert!(what(header(25).extent(0, 64, CAPACITY)).contains("half the ring"));
        assert!(what(header(LENGTH).extent(0, 64, CAPACITY)).contains("half the ring"));
        assert!(what(header(24).extent(8, 32, CAPACITY)).contains("past the write"));
        assert!(what(header(0).extent(16, 16, CAPACITY)).contains("past the write"));
    }

    #[test]
    fn a_view_of_a_conforming_publisher_passes() {
        let mut seen = Seen::default();
        assert_eq!(seen.view(0, 0, 0, CAPACITY), Ok(View { tail: 0, write: 0 }));
        // `before` is older than `write`, which is older than `tail`: both gaps are allowed.
        let view = seen.view(0, 128, 96, CAPACITY).unwrap();
        assert_eq!((view.tail, view.write), (96, 128));
        // The publisher laps the reader between the last two loads.
        let view = seen.view(96, 128, 400, CAPACITY).unwrap();
        assert_eq!((view.tail, view.write), (400, 128));
        assert_eq!(seen.kept(400, 432, "half"), Ok(400));
    }

    #[test]
    fn a_tail_loaded_after_the_write_counter_passes_and_may_be_ahead_of_it() {
        let mut seen = Seen::default();
        seen.newer(128).unwrap();
        assert_eq!(seen.tail(64, CAPACITY), Ok(64));
        // The publisher went on after `write` was loaded: a `tail` above it is no defect here.
        assert_eq!(seen.tail(200, CAPACITY), Ok(200));
        assert!(what(seen.tail(192, CAPACITY)).contains("tail moved"));
        assert!(what(seen.tail(204, CAPACITY)).contains("tail off"));

        let mut seen = Seen::default();
        seen.newer(72).unwrap();
        assert!(what(seen.tail(0, CAPACITY)).contains("more bytes kept"));
        assert_eq!(seen.tail(8, CAPACITY), Ok(8));
    }

    #[test]
    fn a_tail_after_a_lap_is_at_most_a_ring_behind_the_frontier_loaded_before_it() {
        let mut seen = Seen::default();
        seen.newer(64).unwrap();
        seen.frontier(CAPACITY + 8).unwrap();
        assert_eq!(seen.lapped_tail(8, CAPACITY), Ok(8));
        // The publisher went on between the two loads: a `tail` past that `frontier` is no defect.
        assert_eq!(seen.lapped_tail(200, CAPACITY), Ok(200));

        let mut seen = Seen::default();
        seen.newer(64).unwrap();
        seen.frontier(CAPACITY + 8).unwrap();
        assert!(what(seen.lapped_tail(0, CAPACITY)).contains("frontier more than a ring"));
        let mut seen = Seen::default();
        seen.newer(64).unwrap();
        assert!(what(seen.lapped_tail(4, CAPACITY)).contains("tail off"));
    }

    #[test]
    fn counters_no_publisher_stores_are_corrupted() {
        let view = |before, write, tail| Seen::default().view(before, write, tail, CAPACITY);
        assert!(what(view(4, 8, 8)).contains("tail off"));
        assert!(what(view(0, 8, 4)).contains("tail off"));
        assert!(what(view(0, 12, 8)).contains("write counter off"));
        assert!(what(view(16, 16, 8)).contains("tail moved"));
        assert!(what(view(16, 8, 16)).contains("tail past"));
        assert!(what(view(0, 72, 0)).contains("more bytes kept"));
        assert_eq!(view(0, 64, 0).map(|v| v.write), Ok(64));

        let mut seen = Seen::default();
        seen.view(8, 64, 8, CAPACITY).unwrap();
        assert!(what(seen.view(0, 64, 0, CAPACITY)).contains("tail moved"));
        assert!(what(seen.view(8, 56, 8, CAPACITY)).contains("write counter moved"));
    }

    #[test]
    fn a_kept_start_lies_between_the_tail_and_the_write_counter() {
        let mut seen = Seen::default();
        seen.view(16, 64, 16, CAPACITY).unwrap();
        assert_eq!(seen.kept(16, 64, "half"), Ok(16));
        assert_eq!(seen.kept(56, 64, "half"), Ok(56));
        assert_eq!(what(seen.kept(8, 64, "half")), "half");
        assert_eq!(what(seen.kept(64, 64, "half")), "half");
        assert_eq!(what(seen.kept(20, 64, "half")), "half");
        assert!(what(seen.kept(16, 56, "half")).contains("moved backwards"));
        // The write counter this call loaded counts for the next view.
        assert_eq!(seen.kept(72, 80, "half"), Ok(72));
        assert!(what(seen.view(16, 72, 16, CAPACITY)).contains("moved backwards"));

        // A `tail` loaded before this `write` and above it was never stored by a publisher.
        let mut seen = Seen::default();
        seen.newer(64).unwrap();
        seen.tail(96, CAPACITY).unwrap();
        assert!(what(seen.kept(96, 88, "half")).contains("tail past"));
    }
}

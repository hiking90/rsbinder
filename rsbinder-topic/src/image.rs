// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A reader run over a memory image, as the fuzzer's entry point and as the tests' way to read
//! bytes no publisher of this crate writes.
//!
//! An image is topic memory as bytes: the 64-byte header, then the ring. Nothing changes it
//! while it is read, so it stands for a publisher that wrote anything at all and then stopped.
//! The claims checked here are the ones a subscriber makes about such memory: no receive call
//! panics, none copies from outside the ring (the heap allocation is exactly the image, for a
//! sanitizer or Miri to watch), and each does a bounded amount of work.

use std::cell::Cell;

use crate::layout::Heap;
use crate::pool::Loan;
use crate::ring::{Corrupted, Loads, Reader, Start, Taken, RETRIES};

/// What one receive call returned, owning the payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Got {
    Item(u32, Vec<u8>),
    End(Vec<u8>),
    Loan(u32, Loan),
    Empty,
    Lagged(u64),
    Corrupted(&'static str),
}

impl Got {
    pub(crate) fn of(taken: Result<Taken<'_>, Corrupted>) -> Self {
        match taken {
            Ok(Taken::Item { index, record }) => Got::Item(index, record.bytes().to_vec()),
            Ok(Taken::End { payload }) => Got::End(payload.to_vec()),
            Ok(Taken::Loan { index, loan }) => Got::Loan(index, loan),
            Ok(Taken::Empty) => Got::Empty,
            Ok(Taken::Lagged(records)) => Got::Lagged(records),
            Err(Corrupted(what)) => Got::Corrupted(what),
        }
    }
}

/// A reader's view that counts the ring copies made through it.
struct Counted<M> {
    memory: M,
    copies: Cell<u64>,
}

impl<M: Loads> Loads for &Counted<M> {
    fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    fn tail(&self) -> u64 {
        self.memory.tail()
    }

    fn frontier(&self) -> u64 {
        self.memory.frontier()
    }

    fn write(&self) -> u64 {
        self.memory.write()
    }

    fn last(&self) -> u64 {
        self.memory.last()
    }

    fn half(&self) -> u64 {
        self.memory.half()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        self.copies.set(self.copies.get() + 1);
        self.memory.load_ring(position, dst);
    }

    fn fence(&self) {
        self.memory.fence();
    }
}

/// Receive calls made per reader: enough to cross a small ring several times.
const CALLS: usize = 48;

/// [`CALLS`] receives of `image` from `start`, then as many of the newest; none if it is refused.
pub(crate) fn replay(image: &[u8], start: Start) -> Vec<Got> {
    let Some(heap) = Heap::from_image(image) else {
        return Vec::new();
    };
    let counted = Counted {
        memory: heap.loaded(),
        copies: Cell::new(0),
    };
    // Two copies a record, for the laps a call allows and the records it may step over.
    let bound = 2 * (u64::from(RETRIES) + counted.memory.capacity() / 8 + 3);
    let Ok(mut reader) = Reader::attach(&counted, start) else {
        return Vec::new();
    };
    let mut got = Vec::new();
    for call in 0..2 * CALLS {
        counted.copies.set(0);
        let taken = if call < CALLS {
            reader.try_recv()
        } else {
            reader.try_latest()
        };
        got.push(Got::of(taken));
        assert!(
            counted.copies.get() <= bound,
            "a receive call made {} ring copies",
            counted.copies.get()
        );
    }
    got
}

/// [`replay`] from each kind of start. The position of a `Latest` start is the image's `tail`.
pub(crate) fn drive(image: &[u8]) -> usize {
    let Some(tail) = Heap::from_image(image).map(|heap| heap.loaded().tail()) else {
        return 0;
    };
    let latest = Start::Latest { at: tail, index: 0 };
    [Start::Oldest, Start::Last, latest]
        .into_iter()
        .map(|start| replay(image, start).len())
        .sum()
}

#[cfg(all(
    test,
    not(loom),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod tests {
    use super::*;

    const CAPACITY: usize = 64;

    /// An image built without the crate's writer: the layout and the format, stated once more.
    struct Image(Vec<u8>);

    impl Image {
        fn new() -> Self {
            Self(vec![0; 0x40 + CAPACITY])
        }

        fn word(mut self, offset: usize, value: u64) -> Self {
            self.0[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
            self
        }

        /// `write`, and `frontier` where a publisher at rest leaves it: at the same position.
        fn write(self, value: u64) -> Self {
            self.word(0x08, value).frontier(value)
        }

        fn frontier(self, value: u64) -> Self {
            self.word(0x38, value)
        }

        fn tail(self, value: u64) -> Self {
            self.word(0x20, value)
        }

        fn last(self, value: u64) -> Self {
            self.word(0x28, value)
        }

        fn half(self, value: u64) -> Self {
            self.word(0x30, value)
        }

        /// A record at `position`, its bytes wrapping at the ring's end.
        fn record(mut self, position: usize, kind: u32, index: u32, payload: &[u8]) -> Self {
            let mut bytes = (kind << 30 | payload.len() as u32).to_le_bytes().to_vec();
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(payload);
            for (at, byte) in bytes.into_iter().enumerate() {
                self.0[0x40 + (position + at) % CAPACITY] = byte;
            }
            self
        }

        /// A record whose header claims `len` payload bytes, whatever follows it.
        fn claim(mut self, position: usize, len: u32) -> Self {
            self.0[0x40 + position..0x44 + position].copy_from_slice(&len.to_le_bytes());
            self
        }
    }

    fn oldest(image: Image) -> Vec<Got> {
        replay(&image.0, Start::Oldest)
    }

    /// The outcomes up to the first that repeats until the end of the receives.
    fn until_settled(got: &[Got]) -> &[Got] {
        let receives = &got[..CALLS];
        let settled = receives.len() - 1;
        let first = receives
            .iter()
            .position(|outcome| *outcome == receives[settled])
            .unwrap();
        &receives[..=first]
    }

    #[test]
    fn records_of_an_image_are_read_in_order() {
        let image = Image::new()
            .record(0, 0, 7, b"")
            .record(8, 0, 8, b"abc")
            .record(24, 0, 9, b"12345678")
            .write(40)
            .last(24);
        let got = oldest(image);
        assert_eq!(
            until_settled(&got),
            [
                Got::Item(7, Vec::new()),
                Got::Item(8, b"abc".to_vec()),
                Got::Item(9, b"12345678".to_vec()),
                Got::Empty
            ]
        );
        // Every record is read by then, so the newest one is not returned again.
        assert_eq!(got[CALLS], Got::Empty);
    }

    #[test]
    fn a_record_across_the_rings_end_is_read_whole() {
        // Positions past the first lap of a ring of 64: the record at 208 lies at offset 16.
        let image = Image::new()
            .record(208, 0, 30, b"")
            .record(216, 0, 31, b"wrapped!")
            .write(232)
            .tail(208)
            .half(208)
            .last(216);
        let got = oldest(image);
        assert_eq!(
            until_settled(&got),
            [
                Got::Item(30, Vec::new()),
                Got::Item(31, b"wrapped!".to_vec()),
                Got::Empty
            ]
        );
        // 24 bytes of payload from offset 56: eight bytes before the ring's end, sixteen after.
        let payload = b"0123456789abcdefghijklmn";
        let image = Image::new()
            .record(240, 0, 5, payload)
            .write(272)
            .tail(240)
            .half(240)
            .last(240);
        assert_eq!(oldest(image)[0], Got::Item(5, payload.to_vec()));
    }

    #[test]
    fn a_length_no_record_has_is_corrupted() {
        // Half the ring is 32 bytes: 24 of payload is the longest record, 25 is one too many.
        let image = Image::new().claim(0, 25).write(64);
        let got = oldest(image);
        assert_eq!(got[0], Got::Corrupted("record longer than half the ring"));
        assert!(got.iter().all(|outcome| *outcome == got[0]));
        let image = Image::new().claim(0, (1 << 30) - 1).write(64);
        assert_eq!(
            oldest(image)[0],
            Got::Corrupted("record longer than half the ring")
        );
        // A length that fits the ring and not the bytes committed.
        let image = Image::new().claim(0, 24).write(24).last(0);
        assert_eq!(
            oldest(image)[0],
            Got::Corrupted("record reaches past the write counter")
        );
    }

    #[test]
    fn counters_out_of_order_are_corrupted() {
        // A refused start yields no outcome at all: `attach` looks at the counters first.
        assert_eq!(oldest(Image::new().write(8).tail(16)), []);
        assert_eq!(oldest(Image::new().write(72)), []);
        assert_eq!(oldest(Image::new().write(12)), []);
        // `last` is looked at by the calls that want the newest record.
        let image = Image::new().record(0, 0, 0, b"").write(8).last(8);
        let got = oldest(image);
        assert_eq!(got[0], Got::Item(0, Vec::new()));
        assert_eq!(replay(&Image::new().write(8).last(8).0, Start::Last), []);
        // `half` by a reader the publisher has lapped.
        let lapped = |half| {
            let image = Image::new()
                .record(96, 0, 12, b"")
                .write(104)
                .tail(96)
                .last(96)
                .half(half);
            replay(&image.0, Start::Latest { at: 0, index: 0 })
        };
        assert_eq!(
            lapped(96)[..2],
            [Got::Lagged(12), Got::Item(12, Vec::new())]
        );
        assert_eq!(
            lapped(88)[0],
            Got::Corrupted("half outside the kept records")
        );
        assert_eq!(
            lapped(104)[0],
            Got::Corrupted("half outside the kept records")
        );
        assert_eq!(
            lapped(100)[0],
            Got::Corrupted("half outside the kept records")
        );
    }

    #[test]
    fn records_of_a_later_layout_are_stepped_over() {
        let image = Image::new()
            .record(0, 3, 0, b"reserved")
            .record(16, 3, 1, b"")
            .record(24, 0, 2, b"item")
            .record(40, 3, 3, b"tail")
            .write(56)
            .last(40);
        let got = oldest(image);
        // No loss is reported: the indices of the records stepped over are accounted for.
        assert_eq!(
            until_settled(&got),
            [Got::Item(2, b"item".to_vec()), Got::Empty]
        );
        let got = replay(&Image::new().record(0, 3, 0, b"").write(8).0, Start::Oldest);
        assert!(got.iter().all(|outcome| *outcome == Got::Empty));
    }

    #[test]
    fn a_loan_record_names_its_slot_and_one_of_another_length_is_corrupted() {
        let loan = Loan {
            slot: 3,
            len: 100,
            generation: 6,
        };
        let image = Image::new()
            .record(0, 2, 0, &loan.bytes())
            .write(24)
            .last(0);
        let got = oldest(image);
        assert_eq!(until_settled(&got), [Got::Loan(0, loan), Got::Empty]);
        // The newest, again.
        let image = Image::new()
            .record(0, 2, 4, &loan.bytes())
            .write(24)
            .last(0);
        assert_eq!(replay(&image.0, Start::Last)[0], Got::Loan(4, loan));
        for payload in [&b""[..], &[0; 8], &[0; 24]] {
            let image = Image::new().record(0, 2, 0, payload).write(64).last(0);
            let image = image.record(8 + payload.len(), 0, 1, b"");
            let got = oldest(image);
            assert_eq!(got[0], Got::Corrupted("loan record of another length"));
            assert!(got.iter().all(|outcome| *outcome == got[0]));
        }
    }

    #[test]
    fn the_end_record_is_returned_by_every_call() {
        let image = Image::new()
            .record(0, 0, 0, b"one")
            .record(16, 1, 1, b"bye")
            .write(32)
            .last(16);
        let got = oldest(image);
        assert_eq!(got[0], Got::Item(0, b"one".to_vec()));
        assert!(got[1..].iter().all(|o| *o == Got::End(b"bye".to_vec())));
    }

    #[test]
    fn bytes_of_any_kind_end_every_call_within_its_bound() {
        assert_eq!(drive(&[]), 0);
        assert_eq!(drive(&[0xff; 0x47]), 0);
        // A linear congruential generator: the same images on every run and every target.
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (state >> 33) as u8
        };
        for round in 0..if cfg!(miri) { 12 } else { 400 } {
            let mut image: Vec<u8> = (0..0x40 + 8 * (1 + round % 24)).map(|_| next()).collect();
            // Small counters on the grid, so that most images pass the first checks.
            for offset in [0x08, 0x20, 0x28, 0x30] {
                let value = u64::from(next() % 40) * 8;
                image[offset..offset + 8].copy_from_slice(&value.to_ne_bytes());
            }
            drive(&image);
        }
        drive(&[0xff; 0x40 + 64]);
        drive(&[0; 0x40 + 64]);
    }
}

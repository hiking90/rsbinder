// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The two cores over heap memory, which is what Miri can run (it has no memfd and no `mmap`).
//! The mapped types are exercised by `tests/unsync.rs`.

#![cfg(target_pointer_width = "64")]

use std::sync::atomic::AtomicUsize;

use super::*;

// The `cfg` above stands for "has `RoWord64`", which `Heap::reader` views the counters as.
const _: () = assert!(RoWord64::PRESENT);

/// Counters and a ring of `u64` elements on the heap; every run covers whole words.
struct Heap {
    end: AtomicU64,
    write: AtomicU64,
    ring: Box<[AtomicUsize]>,
}

impl Heap {
    fn new(capacity: usize) -> Self {
        Self {
            end: AtomicU64::new(0),
            write: AtomicU64::new(0),
            ring: (0..capacity).map(|_| AtomicUsize::new(0)).collect(),
        }
    }

    fn shape(&self) -> Shape {
        Shape {
            bytes: self.ring.len() as u64 * 8,
            quantum: 8,
        }
    }

    fn ring(&self) -> NonNull<u8> {
        NonNull::from(&*self.ring).cast()
    }

    fn writer(&self) -> WriterCore {
        // SAFETY: atomics of `self`, which outlives every core; a `u64` run covers whole words.
        unsafe {
            WriterCore::new(
                NonNull::from(&self.end),
                NonNull::from(&self.write),
                self.ring(),
                self.shape(),
            )
        }
    }

    fn reader(&self) -> ReaderCore {
        // SAFETY: as in `writer`; the reader only loads.
        unsafe {
            ReaderCore::new(
                NonNull::from(&self.end).cast(),
                NonNull::from(&self.write).cast(),
                self.ring(),
                self.shape(),
            )
        }
    }
}

fn put(writer: &mut WriterCore, items: &[u64]) {
    let mut regions = writer.begin::<u64>(items.len()).unwrap();
    regions.write_at(0, items).unwrap();
    writer.finish::<u64>(items.len()).unwrap();
}

fn take(reader: &mut ReaderCore, out: &mut [u64]) -> UnsyncRead {
    reader.take(ring::bytes_of_mut(out)).unwrap()
}

#[test]
fn elements_come_out_in_the_order_they_went_in() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    let mut out = [0u64; 3];
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Empty);
    // 21 elements through a ring of 8: the positions wrap twice.
    for round in 0..7u64 {
        put(&mut writer, &[round * 3, round * 3 + 1, round * 3 + 2]);
        assert_eq!(reader.pending().unwrap(), 24);
        assert_eq!(take(&mut reader, &mut out), UnsyncRead::Done);
        assert_eq!(out, [round * 3, round * 3 + 1, round * 3 + 2]);
    }
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Empty);
}

#[test]
fn a_lapped_reader_is_told_how_many_elements_it_skipped() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    for value in 0..20u64 {
        put(&mut writer, &[value]);
    }
    let mut out = [0u64; 4];
    // Half the ring is kept: the reader resumes at element 16.
    assert_eq!(
        take(&mut reader, &mut out),
        UnsyncRead::Lost { elements: 16 }
    );
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Done);
    assert_eq!(out, [16, 17, 18, 19]);
}

#[test]
fn a_reservation_makes_the_reader_discard_what_it_covers() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    put(&mut writer, &[0, 1, 2, 3, 4, 5, 6]);
    // Nothing is committed past element 6, but three more are reserved: elements 0 and 1 go.
    writer.reserve(24).unwrap();
    let mut out = [0u64; 1];
    // Half the ring is kept: elements 3 to 6.
    assert_eq!(
        take(&mut reader, &mut out),
        UnsyncRead::Lost { elements: 3 }
    );
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Done);
    assert_eq!(out, [3]);
}

#[test]
fn a_reservation_into_the_kept_half_leaves_the_reader_at_the_writer() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    put(&mut writer, &[0, 1, 2, 3, 4, 5, 6]);
    // Five reserved elements leave room for three, fewer than the half a lapped reader keeps.
    writer.reserve(40).unwrap();
    let mut out = [0u64; 1];
    assert_eq!(
        take(&mut reader, &mut out),
        UnsyncRead::Lost { elements: 7 }
    );
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Empty);
}

#[test]
fn the_region_end_stays_where_a_partly_committed_write_put_it() {
    let heap = Heap::new(16);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    let filler: Vec<u64> = (0..8).collect();
    put(&mut writer, &filler);
    put(&mut writer, &filler);
    let mut out = [0u64; 4];
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Done);

    // write = 16 elements, read = 4: fill a reservation of eight, commit two of them.
    let mut regions = writer.begin::<u64>(8).unwrap();
    regions.write_at(0, &[100; 8]).unwrap();
    writer.finish::<u64>(2).unwrap();
    // A shorter reservation from the new write counter does not lower the end.
    writer.reserve(8).unwrap();
    assert_eq!(heap.end.load(Ordering::Relaxed), 24 * 8);
    assert_eq!(heap.write.load(Ordering::Relaxed), 18 * 8);

    // Elements 4 to 7 lie under the six that were written and not committed.
    assert_eq!(
        take(&mut reader, &mut out),
        UnsyncRead::Lost { elements: 6 }
    );
    let mut rest = [0u64; 8];
    assert_eq!(take(&mut reader, &mut rest), UnsyncRead::Done);
    assert_eq!(rest, [2, 3, 4, 5, 6, 7, 100, 100]);

    // The rest of the reservation can still be committed, and a new one moves the end on.
    writer.commit(48).unwrap();
    assert!(matches!(writer.commit(8), Err(Error::BadValue(_))));
    writer.reserve(8).unwrap();
    assert_eq!(heap.end.load(Ordering::Relaxed), 25 * 8);
}

#[test]
fn latest_copies_the_newest_and_consumes_the_rest() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    let mut out = [0u64; 3];
    assert_eq!(reader.take_latest(ring::bytes_of_mut(&mut out)).unwrap(), 0);
    put(&mut writer, &[1, 2]);
    assert_eq!(
        reader.take_latest(ring::bytes_of_mut(&mut out)).unwrap(),
        16
    );
    assert_eq!(out[..2], [1, 2]);
    for value in 3..30u64 {
        put(&mut writer, &[value]);
    }
    assert_eq!(
        reader.take_latest(ring::bytes_of_mut(&mut out)).unwrap(),
        24
    );
    assert_eq!(out, [27, 28, 29]);
    assert_eq!(reader.pending().unwrap(), 0);
    assert_eq!(take(&mut reader, &mut out[..1]), UnsyncRead::Empty);
}

#[test]
fn a_counter_that_moved_backwards_ends_the_reader() {
    let heap = Heap::new(8);
    let (mut writer, mut reader) = (heap.writer(), heap.reader());
    put(&mut writer, &[1, 2, 3]);
    let mut out = [0u64; 1];
    assert_eq!(take(&mut reader, &mut out), UnsyncRead::Done);
    // What libfmq's constructor does to both words with `resetPointers = true`.
    heap.write.store(0, Ordering::Release);
    heap.end.store(0, Ordering::Release);
    let corrupted = Error::Corrupted("write counter moved backwards");
    assert_eq!(reader.take(ring::bytes_of_mut(&mut out)), Err(corrupted));
    // The writer's next write restores both words; the reader stays failed.
    put(&mut writer, &[4]);
    assert_eq!(heap.write.load(Ordering::Relaxed), 32);
    assert_eq!(reader.pending(), Err(corrupted));
    let latest = reader.take_latest(ring::bytes_of_mut(&mut out));
    assert_eq!(latest, Err(corrupted));
}

#[test]
fn a_read_longer_than_the_ring_is_refused() {
    let heap = Heap::new(4);
    let mut reader = heap.reader();
    let mut out = [0u64; 5];
    assert!(matches!(
        reader.take(ring::bytes_of_mut(&mut out)),
        Err(Error::BadValue(_))
    ));
    // The latest read takes what fits and is not an error.
    assert_eq!(reader.take_latest(ring::bytes_of_mut(&mut out)).unwrap(), 0);
    // Lapped, it takes one ring: the copy starts at element 6 and wraps.
    let mut writer = heap.writer();
    for value in 0..10u64 {
        put(&mut writer, &[value]);
    }
    assert_eq!(
        reader.take_latest(ring::bytes_of_mut(&mut out)).unwrap(),
        32
    );
    assert_eq!(out[..4], [6, 7, 8, 9]);
}

#[test]
#[should_panic(expected = "a copy longer than the ring")]
fn a_copy_longer_than_the_ring_panics() {
    let heap = Heap::new(4);
    // One element more than the ring holds.
    heap.reader().copy_out(0, &mut [0u8; 40]);
}

/// The order `take`'s second look rests on. Decides under Miri; x86 keeps it with any copy.
#[test]
fn a_copy_that_caught_a_write_is_followed_by_a_view_of_its_reservation() {
    let (repeats, rounds) = if cfg!(miri) { (1, 60u64) } else { (200, 400) };
    for _ in 0..repeats {
        let heap = Heap::new(4);
        std::thread::scope(|s| {
            let heap = &heap;
            let writer = s.spawn(move || {
                let mut writer = heap.writer();
                for value in 0..rounds {
                    let mut regions = writer.begin::<u64>(1).unwrap();
                    regions.write_at(0, &[value]).unwrap();
                    // The reader runs here: the element is in the ring and not committed.
                    std::thread::yield_now();
                    writer.finish::<u64>(1).unwrap();
                }
            });
            s.spawn(move || {
                let reader = heap.reader();
                let mut cells = [0u64; 4];
                // A writer that panicked is finished too, and the scope reports it.
                while !writer.is_finished() {
                    reader.copy_out(0, ring::bytes_of_mut(&mut cells));
                    let view = reader.view().unwrap();
                    // Element `e` went into the cell of element `e - 4`: that position is lapped.
                    let newest = cells.into_iter().max().unwrap();
                    if let Some(overwritten) = newest.checked_sub(4) {
                        let lapped = view.lapped(overwritten * 8, heap.shape()).is_some();
                        assert!(lapped, "element {newest} with a view of {view:?}");
                    }
                    std::thread::yield_now();
                }
            });
        });
    }
}

/// Exists for Miri and its weak-memory emulation: natively x86 keeps the order with any copy.
#[test]
fn a_reader_racing_the_writer_gets_whole_elements_in_order() {
    let (repeats, writes) = if cfg!(miri) {
        (1, 120u64)
    } else {
        (50, 20_000)
    };
    for _ in 0..repeats {
        // Four elements: one write of three laps a reader that is two behind.
        let heap = Heap::new(4);
        std::thread::scope(|s| {
            let heap = &heap;
            let writer = s.spawn(move || {
                let mut writer = heap.writer();
                let (mut next, mut round) = (0u64, 0u64);
                while next < writes {
                    // One to three elements, so that the wrap falls inside a write too.
                    let n = round % 3 + 1;
                    round += 1;
                    let items: Vec<u64> = (next..next + n).collect();
                    put(&mut writer, &items);
                    next += n;
                }
            });
            s.spawn(move || {
                let mut reader = heap.reader();
                let mut expected = 0u64;
                let mut out = [0u64; 2];
                while expected < writes - 3 {
                    match take(&mut reader, &mut out) {
                        UnsyncRead::Done => {
                            assert_eq!(out, [expected, expected + 1], "an overwritten element");
                            expected += 2;
                        }
                        UnsyncRead::Lost { elements } => expected += elements,
                        // Also the way out when the writer panicked; the scope reports that.
                        UnsyncRead::Empty if writer.is_finished() => break,
                        UnsyncRead::Empty => std::thread::yield_now(),
                    }
                }
            });
        });
    }
}

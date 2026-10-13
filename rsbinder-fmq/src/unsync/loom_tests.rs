// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The unsynchronized reader's rule as a loom model: a ring of one-byte cells, a writer that
//! announces, stores and commits as `WriterCore` does, and a reader that judges the counters
//! with the functions `ReaderCore` calls (`rules.rs`). It checks the rule — the order of the
//! loads and the judgement after the copy — and does not run the copies of `Run`; the test that
//! does is `tests.rs` under Miri. Run: see `Cargo.toml` (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

use super::rules::{Shape, View};

const CELLS: u64 = 2;
const SHAPE: Shape = Shape {
    bytes: CELLS,
    quantum: 1,
};

struct Queue {
    end: AtomicU64,
    write: AtomicU64,
    ring: [AtomicUsize; CELLS as usize],
}

/// What the model leaves out of the real reader, one rule at a time.
#[derive(Clone, Copy, PartialEq)]
enum Reader {
    Whole,
    /// No judgement after the copy.
    NoSecondLook,
    /// `write` loaded before `end`.
    WriteFirst,
}

impl Queue {
    /// `RoWord64::load_acquire` on both words, in `ReaderCore::view`'s order.
    fn view(&self, seen: u64, kind: Reader) -> View {
        let load = |word: &AtomicU64| {
            let value = word.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            value
        };
        let (end, write) = if kind == Reader::WriteFirst {
            let write = load(&self.write);
            (load(&self.end), write)
        } else {
            let end = load(&self.end);
            (end, load(&self.write))
        };
        View::new(seen, write, end, SHAPE).expect("a view no conforming writer produces")
    }
}

/// The writer makes `writes` single-cell writes; cell `p` holds `p + 1`. The reader reads twice.
fn every_cell_read_is_the_one_written_there(writes: u64, kind: Reader) {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.check(move || {
        let queue = Arc::new(Queue {
            end: AtomicU64::new(0),
            write: AtomicU64::new(0),
            ring: [AtomicUsize::new(0), AtomicUsize::new(0)],
        });
        let writer = {
            let queue = queue.clone();
            thread::spawn(move || {
                for position in 0..writes {
                    queue.end.store(position + 1, Ordering::Release);
                    let cell = &queue.ring[(position % CELLS) as usize];
                    cell.store(position as usize + 1, Ordering::Release);
                    queue.write.store(position + 1, Ordering::Release);
                }
            })
        };
        let (mut read, mut seen) = (0u64, 0u64);
        for _ in 0..2 {
            let view = queue.view(seen, kind);
            seen = view.write;
            if let Some(resume) = view.lapped(read, SHAPE) {
                assert!(resume > read && resume <= view.write);
                read = resume;
                continue;
            }
            if view.write == read {
                continue;
            }
            let value = queue.ring[(read % CELLS) as usize].load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if kind != Reader::NoSecondLook {
                let after = queue.view(seen, kind);
                seen = after.write;
                if let Some(resume) = after.lapped(read, SHAPE) {
                    read = resume;
                    continue;
                }
            }
            assert_eq!(value as u64, read + 1, "an overwritten cell was returned");
            read += 1;
        }
        writer.join().unwrap();
    });
}

#[test]
fn loom_unsync_reader_returns_only_what_was_written_at_its_position() {
    every_cell_read_is_the_one_written_there(3, Reader::Whole);
}

#[test]
#[should_panic(expected = "an overwritten cell was returned")]
fn loom_unsync_reader_without_the_second_look_returns_an_overwritten_cell() {
    every_cell_read_is_the_one_written_there(3, Reader::NoSecondLook);
}

#[test]
#[should_panic(expected = "a view no conforming writer produces")]
fn loom_unsync_reader_that_loads_write_first_sees_too_long_a_reservation() {
    every_cell_read_is_the_one_written_there(3, Reader::WriteFirst);
}

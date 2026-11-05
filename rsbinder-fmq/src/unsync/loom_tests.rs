// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The unsynchronized reader's rule as a loom model: a ring of one-byte cells, a writer that
//! announces, fences, stores and commits as `WriterCore` does, and a reader that judges the
//! counters with the functions `ReaderCore` calls (`rules.rs`). It checks the rule — the order of
//! the loads and the judgement after the copy — and does not run the copies of `Bytes`; the test
//! that does is `tests.rs` under Miri. A write of two cells has one fence and two `Relaxed` stores,
//! which come in no order. Run: see `Cargo.toml` (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicU64, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

use super::rules::{Shape, View};

/// The ring, and how many cells one write and one read take.
#[derive(Clone, Copy)]
struct Size {
    cells: u64,
    at_once: u64,
}

const ONE_AT_ONCE: Size = Size {
    cells: 2,
    at_once: 1,
};
const TWO_AT_ONCE: Size = Size {
    cells: 3,
    at_once: 2,
};

impl Size {
    fn shape(self) -> Shape {
        Shape {
            bytes: self.cells,
            quantum: 1,
        }
    }
}

struct Queue {
    end: AtomicU64,
    write: AtomicU64,
    ring: Vec<AtomicUsize>,
    size: Size,
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

/// What the model leaves out of the real writer.
#[derive(Clone, Copy, PartialEq)]
enum Writer {
    Whole,
    /// No release fence between `end` and the cells.
    NoFence,
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
        View::new(seen, write, end, self.size.shape())
            .expect("a view no conforming writer produces")
    }

    fn cell(&self, position: u64) -> &AtomicUsize {
        &self.ring[(position % self.size.cells) as usize]
    }
}

/// The writer makes `writes` writes; cell `p` holds `p + 1`. The reader reads twice.
fn every_cell_read_is_the_one_written_there(size: Size, writes: u64, writer: Writer, kind: Reader) {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.check(move || {
        let queue = Arc::new(Queue {
            end: AtomicU64::new(0),
            write: AtomicU64::new(0),
            ring: (0..size.cells).map(|_| AtomicUsize::new(0)).collect(),
            size,
        });
        let writing = {
            let queue = queue.clone();
            thread::spawn(move || {
                for nth in 0..writes {
                    let position = nth * size.at_once;
                    let end = position + size.at_once;
                    queue.end.store(end, Ordering::Release);
                    if writer != Writer::NoFence {
                        fence(Ordering::Release);
                    }
                    for at in position..end {
                        queue.cell(at).store(at as usize + 1, Ordering::Relaxed);
                    }
                    queue.write.store(end, Ordering::Release);
                }
            })
        };
        let (mut read, mut seen) = (0u64, 0u64);
        for _ in 0..2 {
            let view = queue.view(seen, kind);
            seen = view.write;
            if let Some(resume) = view.lapped(read, size.shape()) {
                assert!(resume > read && resume <= view.write);
                read = resume;
                continue;
            }
            if view.write - read < size.at_once {
                continue;
            }
            let copy: Vec<usize> = (read..read + size.at_once)
                .map(|at| queue.cell(at).load(Ordering::Relaxed))
                .collect();
            fence(Ordering::Acquire);
            if kind != Reader::NoSecondLook {
                let after = queue.view(seen, kind);
                seen = after.write;
                if let Some(resume) = after.lapped(read, size.shape()) {
                    read = resume;
                    continue;
                }
            }
            for (at, value) in (read..).zip(copy) {
                assert_eq!(value as u64, at + 1, "an overwritten cell was returned");
            }
            read += size.at_once;
        }
        writing.join().unwrap();
    });
}

#[test]
fn loom_unsync_reader_returns_only_what_was_written_at_its_position() {
    every_cell_read_is_the_one_written_there(ONE_AT_ONCE, 3, Writer::Whole, Reader::Whole);
}

#[test]
fn loom_unsync_reader_returns_only_what_was_written_with_the_cells_of_a_write_in_no_order() {
    every_cell_read_is_the_one_written_there(TWO_AT_ONCE, 3, Writer::Whole, Reader::Whole);
}

#[test]
#[should_panic(expected = "an overwritten cell was returned")]
fn loom_unsync_reader_without_the_second_look_returns_an_overwritten_cell() {
    every_cell_read_is_the_one_written_there(ONE_AT_ONCE, 3, Writer::Whole, Reader::NoSecondLook);
}

#[test]
#[should_panic(expected = "a view no conforming writer produces")]
fn loom_unsync_reader_that_loads_write_first_sees_too_long_a_reservation() {
    every_cell_read_is_the_one_written_there(ONE_AT_ONCE, 3, Writer::Whole, Reader::WriteFirst);
}

#[test]
#[should_panic(expected = "an overwritten cell was returned")]
fn loom_unsync_reader_of_a_writer_without_its_fence_returns_an_overwritten_cell() {
    every_cell_read_is_the_one_written_there(TWO_AT_ONCE, 3, Writer::NoFence, Reader::Whole);
}

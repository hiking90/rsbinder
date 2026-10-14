// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The writer and the reader of `ring.rs`, run by loom over a model of topic memory: the words
//! are loom's atomics, with the orderings `layout.rs` gives the mapped ones. What loom checks is
//! the order the protocol rests on, on every interleaving it explores: `tail` is stored before
//! the bytes, the bytes with `Release`, the reader's counter loads are followed by an acquire
//! fence, and an acquire fence comes between a copy and the load of `tail` that judges it, so
//! that `tail` covers every record whose bytes the copy saw. The three flawed memories show that
//! the check is not vacuous. Run: see `Cargo.toml` (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

use super::*;

/// A ring of four words: two records of a header and one payload word.
const WORDS: u64 = 4;

/// What a memory leaves out of the orderings the mapped one has.
#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// The ring's bytes are stored `Relaxed`.
    RelaxedRing,
    /// No load of the reader, of a counter or of the ring, is followed by an acquire fence.
    NoFence,
    /// The counters' loads keep theirs; `Loads::fence`, between a copy and its check, is gone.
    NoCopyFence,
}

struct Memory {
    region_end: AtomicU64,
    write: AtomicU64,
    tail: AtomicU64,
    last: AtomicU64,
    half: AtomicU64,
    ring: [AtomicU64; WORDS as usize],
    flaw: Flaw,
}

impl Memory {
    fn new(flaw: Flaw) -> Arc<Self> {
        Arc::new(Self {
            region_end: AtomicU64::new(0),
            write: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            last: AtomicU64::new(0),
            half: AtomicU64::new(0),
            ring: [(); WORDS as usize].map(|()| AtomicU64::new(0)),
            flaw,
        })
    }

    fn word(&self, position: u64, nth: usize) -> &AtomicU64 {
        &self.ring[((position / 8 + nth as u64) % WORDS) as usize]
    }

    /// What follows a counter's load (`RoWord64::load_acquire`), and what `Loads::fence` is.
    fn acquire(&self) {
        if self.flaw != Flaw::NoFence {
            fence(Ordering::Acquire);
        }
    }

    fn load(&self, word: &AtomicU64) -> u64 {
        let value = word.load(Ordering::Relaxed);
        self.acquire();
        value
    }
}

impl Stores for Arc<Memory> {
    fn capacity(&self) -> u64 {
        WORDS * 8
    }

    fn store_tail(&self, position: u64) {
        self.tail.store(position, Ordering::Release);
    }

    fn store_region_end(&self, position: u64) {
        self.region_end.store(position, Ordering::Release);
    }

    fn store_write(&self, position: u64) {
        self.write.store(position, Ordering::Release);
    }

    fn store_last(&self, position: u64) {
        self.last.store(position, Ordering::Release);
    }

    fn store_half(&self, position: u64) {
        self.half.store(position, Ordering::Release);
    }

    fn bump_seq(&self) {}

    fn store_ring(&self, position: u64, src: &[u8]) {
        let order = match self.flaw {
            Flaw::RelaxedRing => Ordering::Relaxed,
            _ => Ordering::Release,
        };
        for (nth, bytes) in src.chunks_exact(8).enumerate() {
            let value = u64::from_ne_bytes(bytes.try_into().expect("a whole word"));
            self.word(position, nth).store(value, order);
        }
    }
}

impl Loads for Arc<Memory> {
    fn capacity(&self) -> u64 {
        WORDS * 8
    }

    fn tail(&self) -> u64 {
        self.load(&self.tail)
    }

    fn write(&self) -> u64 {
        self.load(&self.write)
    }

    fn last(&self) -> u64 {
        self.load(&self.last)
    }

    fn half(&self) -> u64 {
        self.load(&self.half)
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        for (nth, bytes) in dst.chunks_exact_mut(8).enumerate() {
            let value = self.word(position, nth).load(Ordering::Relaxed);
            bytes.copy_from_slice(&value.to_ne_bytes());
        }
    }

    fn fence(&self) {
        if self.flaw != Flaw::NoCopyFence {
            self.acquire();
        }
    }
}

fn payload(index: u32) -> [u8; 8] {
    [index as u8 + 1; 8]
}

/// The writer puts `puts` records; the reader receives twice and judges what it is given.
fn every_record_returned_is_the_one_written(flaw: Flaw, puts: u32) {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.check(move || {
        let memory = Memory::new(flaw);
        let writer = {
            let memory = memory.clone();
            thread::spawn(move || {
                let mut writer = Writer::new(memory);
                for index in 0..puts {
                    writer.put(Kind::Item, &payload(index)).unwrap();
                }
            })
        };
        let start = Start::Latest { at: 0, index: 0 };
        let mut reader = Reader::attach(memory, start).unwrap();
        let mut next = 0;
        for _ in 0..2 {
            match reader.try_recv() {
                Ok(Taken::Item {
                    index,
                    payload: got,
                }) if index == next => {
                    assert_eq!(got, payload(index), "a record that was not written there");
                    next += 1;
                }
                Ok(Taken::Lagged(lost)) => next += lost as u32,
                Ok(Taken::Empty) => {}
                Err(Corrupted(what)) => panic!("a publisher that conforms failed a check: {what}"),
                other => panic!("a record that was not written there: {other:?}"),
            }
        }
        writer.join().unwrap();
    });
}

#[test]
fn loom_topic_reader_returns_only_records_the_writer_wrote_there() {
    every_record_returned_is_the_one_written(Flaw::None, 3);
}

#[test]
#[should_panic(expected = "a record that was not written there")]
fn loom_topic_reader_over_relaxed_ring_stores_returns_a_torn_record() {
    every_record_returned_is_the_one_written(Flaw::RelaxedRing, 3);
}

#[test]
#[should_panic(expected = "a record that was not written there")]
fn loom_topic_reader_without_the_fences_returns_a_torn_record() {
    every_record_returned_is_the_one_written(Flaw::NoFence, 3);
}

// The one fence the reader adds of its own, after the copy: its check needs it.
#[test]
#[should_panic(expected = "a record that was not written there")]
fn loom_topic_reader_without_the_fence_before_its_check_returns_a_torn_record() {
    every_record_returned_is_the_one_written(Flaw::NoCopyFence, 3);
}

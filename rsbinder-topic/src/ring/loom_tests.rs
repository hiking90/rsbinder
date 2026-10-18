// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The writer and the reader of `ring.rs`, run by loom over a model of topic memory: the words
//! are loom's atomics, with the orderings `layout.rs` gives the mapped ones. What loom checks is
//! the order the protocol rests on, on every interleaving it explores: `frontier` is stored
//! before each chunk's bytes, the bytes with `Release`, the reader's counter loads are followed
//! by an acquire fence, and an acquire fence comes between a chunk's copy and the load of
//! `frontier` that judges it, so that `frontier` covers every byte the copy saw of a newer
//! record. Records are two chunks long for the reader (`CHUNK` is 8 under loom), so a record whose
//! head is written over after its first chunk passed is judged by its second. The four flawed
//! memories show that the check is not vacuous. Run: see `Cargo.toml` (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicU64, Ordering};
use loom::sync::Arc;
use loom::thread;

use super::*;

/// A ring of six words: two records of a header and two payload words.
const WORDS: u64 = 6;

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
    /// `frontier` lags a chunk: each store writes the value the one before it was given.
    LateFrontier,
}

struct Memory {
    region_end: AtomicU64,
    write: AtomicU64,
    tail: AtomicU64,
    frontier: AtomicU64,
    /// The value [`Flaw::LateFrontier`] holds back for the next store.
    held_frontier: AtomicU64,
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
            frontier: AtomicU64::new(0),
            held_frontier: AtomicU64::new(0),
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

    fn store_frontier(&self, position: u64) {
        let position = match self.flaw {
            // The writer's own word: only its thread touches it.
            Flaw::LateFrontier => self.held_frontier.swap(position, Ordering::Relaxed),
            _ => position,
        };
        self.frontier.store(position, Ordering::Release);
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

    fn bump_seq(&self) -> u32 {
        0
    }

    fn wake_seq(&self) {}

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

    fn frontier(&self) -> u64 {
        self.load(&self.frontier)
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

fn payload(index: u32) -> [u8; 16] {
    [index as u8 + 1; 16]
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
                Ok(Taken::Item { index, record }) if index == next => {
                    assert_eq!(
                        record.bytes(),
                        payload(index),
                        "unsound: a record that was not written there"
                    );
                    next += 1;
                }
                Ok(Taken::Lagged(lost)) => next += lost as u32,
                Ok(Taken::Empty) => {}
                Err(Corrupted(what)) => {
                    panic!("unsound: a publisher that conforms failed a check: {what}")
                }
                other => panic!("unsound: a record that was not written there: {other:?}"),
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

// Without them a `write` can be seen with an older `frontier`, which a check finds first.
#[test]
#[should_panic(expected = "unsound:")]
fn loom_topic_reader_without_the_fences_returns_a_torn_record_or_fails_a_check() {
    every_record_returned_is_the_one_written(Flaw::NoFence, 3);
}

// The one fence the reader adds of its own, after the copy: its check needs it.
#[test]
#[should_panic(expected = "a record that was not written there")]
fn loom_topic_reader_without_the_fence_before_its_check_returns_a_torn_record() {
    every_record_returned_is_the_one_written(Flaw::NoCopyFence, 3);
}

// `frontier` has to reach past a chunk before the chunk's bytes are stored.
#[test]
#[should_panic(expected = "a record that was not written there")]
fn loom_topic_reader_behind_a_late_frontier_returns_a_torn_record() {
    every_record_returned_is_the_one_written(Flaw::LateFrontier, 3);
}

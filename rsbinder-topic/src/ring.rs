// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The record ring: what the publisher writes into topic memory, and how a subscriber reads it
//! while it is being overwritten.
//!
//! # Records
//!
//! The ring is a run of bytes whose length, the capacity, is a multiple of 8. A position is a
//! count of bytes written since the topic was made; the byte at position `p` lies at `p %
//! capacity`. A record starts on a multiple of 8:
//!
//! ```text
//! u32 LE   kind << 30 | length      length: the payload's bytes, without the padding
//! u32 LE   index                    records the publisher wrote before this one, mod 2^32
//! payload, then zeros up to the next multiple of 8
//! ```
//!
//! Kind 0 is an item, kind 1 the end of the topic, after which the publisher writes nothing, and
//! kind 2 a loan: a value in a slot of the pool, which the record names in 16 bytes (`pool.rs`).
//! Kind 3 belongs to a later version of the layout; a reader of this version steps over it by its
//! length. One record is at most half the capacity.
//!
//! # The publisher
//!
//! There is one. It never waits for a reader here and never reads the shared words (a topic with
//! backpressure waits before it writes, `backpressure.rs`); what it needs to know it keeps
//! to itself (the positions of the records still in the ring). To write a record of `r` bytes at
//! position `w` it stores, in this order:
//!
//! 1. `tail`: the start of the oldest record that survives, so that `w + r - tail <= capacity`.
//!    From here on a reader treats everything below `tail` as gone.
//! 2. `region_end = w + r`, the word a libfmq reader of the unsynchronized flavor judges by.
//! 3. The record's bytes, with `Release` stores, a [`CHUNK`] at a time counted from `w`: before
//!    each chunk, `frontier` = the chunk's end. The byte at `p` of the ring before this lap is
//!    written over from position `p + capacity` on, so it is untouched while `frontier <= p +
//!    capacity`.
//! 4. `write = w + r`. The record is committed.
//! 5. `last = w`, the start of the newest record.
//! 6. `half`: the start of the oldest record at or above `write - capacity / 2`.
//!
//! Between any two of these stores `tail <= half <= last < write`, `write - tail <= capacity`
//! and `write <= frontier` hold, so a publisher that dies there leaves memory a reader can finish
//! reading.
//!
//! # A subscriber
//!
//! It keeps its own read position, in its own memory, and only loads the shared words. A record
//! is copied out a [`CHUNK`] of payload at a time, and each piece is judged once copied: an
//! acquire fence, then one load of `frontier`, and if `frontier` is past the piece's first byte
//! plus the capacity, the piece may hold bytes of a newer record and the record is dropped. The
//! first piece is judged from the record's start, with the header loaded before it. That check
//! is ordered, not merely likely to work: the publisher's stores are `Release`, so a reader that
//! copied even one byte of a chunk of an overwriting record then loads a `frontier` at least as
//! new as the one stored before that chunk. The publisher writes the ring in the order the
//! reader copies it, so a piece judged whole stays a whole copy whatever is written over it
//! later: a record whose head the publisher writes over behind the reader is still returned, and
//! a copy the publisher overtakes stops within a chunk of where it was overtaken. A header is
//! believed before its judgment only when it is what the reader expects (the next index, or any
//! index while the reader knows none, before the first record of a reader that attached with no
//! index, whose copy is judged from its start; a length that fits what is committed); any other
//! header is judged by `frontier` first, so a torn one is not taken for corruption.
//!
//! A chunk is 64 KiB. A move of `frontier` is a store and a judgment an acquire fence and a load,
//! small beside storing or copying that much, and a record no longer than a chunk takes one of
//! each.
//!
//! The reader holds the newest `write` it loaded and loads it again only once it has read up to
//! it: every record below it is committed, and the acquire after that load orders the copies of
//! them. A received record thus loads `write` at most once, besides the `frontier` of its
//! judgments. The checks on the counters compare each with the reader's own state, or two loaded
//! in the order that makes the bound hold for a publisher that follows the protocol:
//! `write - tail <= capacity` with `write` loaded first, `tail <= write` with `tail` first (at
//! attach, for `last`, and after a lap, where `half` is checked against both), `write <= frontier`
//! with `write` first (at each judgment).
//!
//! A reader the publisher has lapped resumes at `half`. Resuming at `tail` would put it on the
//! record the next publish overwrites; from `half` the publisher has to write half a ring before
//! it catches up again. A drain of the records before an end ([`Reader::stop_at`]) resumes at
//! `tail` instead, which keeps the most of them, until two laps in a row find it still on the
//! record such a resume put it on, overtaken before it moved past it: from then on it resumes at
//! `half`, as that publisher writes over the oldest record before the drain reaches it. One such
//! lap is not enough, as a drain that returns a `Lagged` first stays on the oldest record, which
//! a single commit before its next receive writes over. A publisher that has stopped writing
//! overtakes nothing at or after its `tail`, so the drain then reads every record from `tail` to
//! its end. A drain gives up where a resume would reach its end.
//! A copy the publisher overtook is a lap like any other: the header loaded before it moves the
//! reader nowhere, as its bytes may be the new lap's. The `tail` a lapped reader resumes by is
//! checked to lie no more than a ring behind the `frontier` that found the lap, so the record
//! there is past every byte that judgment found written over, and ahead of the reader. How many
//! records were skipped is the distance between the index of the record the reader owed and the
//! index of the first record judged whole after the jump, so the losses it reports add up to the
//! records it did not return.
//!
//! The publisher's memory is not trusted. Lengths and counters are checked before use
//! ([`Corrupted`]); the place a copy starts is a position modulo the capacity and its length is
//! bounded by half the capacity, so no value in shared memory makes a copy leave the ring; and a
//! call that resumes [`RETRIES`] times and is lapped again gives up, so a publisher that keeps
//! moving `frontier` cannot hold a caller.

use std::collections::VecDeque;
use std::num::NonZeroUsize;

mod cursor;
mod rules;

use cursor::Cursor;
use rules::{Header, Seen, View};

use crate::pool::Loan;
use crate::Record;

/// How often one receive call resumes after being lapped before it returns without a record;
/// in `try_latest`, a record of a kind it skips takes one of them too.
pub(crate) const RETRIES: u32 = 4;

/// Bytes between two moves of `frontier`, and the most a reader copies before it judges them.
#[cfg(not(any(test, loom)))]
const CHUNK: usize = 64 * 1024;
/// Smaller than the records of the race tests (24 bytes), so that they copy in two chunks.
#[cfg(all(test, not(loom)))]
const CHUNK: usize = 16;
/// Half the payload of loom's records (16 bytes), so that a reader judges two chunks of each.
#[cfg(loom)]
const CHUNK: usize = 8;

/// The kinds of record this version writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// One published value.
    Item = 0,
    /// The topic's last record; its payload says how it ended.
    End = 1,
    /// A value in a slot of the pool: the payload is a [`Loan`].
    Loan = 2,
}

/// Topic memory as the publisher reaches it. Each store is `Release`.
pub(crate) trait Stores {
    /// Bytes in the ring: a multiple of 8, never zero.
    fn capacity(&self) -> u64;
    fn store_tail(&self, position: u64);
    fn store_region_end(&self, position: u64);
    /// How far the record being written reaches once the chunk about to be stored is.
    fn store_frontier(&self, position: u64);
    fn store_write(&self, position: u64);
    fn store_last(&self, position: u64);
    fn store_half(&self, position: u64);
    /// Add one to the word a waiting subscriber sleeps on; the value it holds now.
    fn bump_seq(&self) -> u32;
    /// Wake every subscriber that sleeps on that word: one system call, however many sleep.
    fn wake_seq(&self);
    /// Copy `src` to `position`, wrapping at the ring's end. Both are multiples of 8.
    fn store_ring(&self, position: u64, src: &[u8]);
}

/// Topic memory as a subscriber reaches it. A counter's load orders the caller as an `Acquire`
/// load does; the ring's loads do not, and [`fence`](Loads::fence) orders them.
pub(crate) trait Loads {
    /// Bytes in the ring: a multiple of 8, never zero.
    fn capacity(&self) -> u64;
    fn tail(&self) -> u64;
    fn frontier(&self) -> u64;
    fn write(&self) -> u64;
    fn last(&self) -> u64;
    fn half(&self) -> u64;
    /// Fill `dst` from `position`, wrapping at the ring's end, with `Relaxed` loads. Both are
    /// multiples of 8.
    fn load_ring(&self, position: u64, dst: &mut [u8]);
    /// [`load_ring`](Self::load_ring) of `len` bytes appended to `dst`, not zeroed first.
    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>);
    /// An acquire fence: the loads that follow come after the stores the ring's loads saw.
    fn fence(&self);
}

/// A record's parts stored in whole words, the last padded; `frontier` moved before each [`CHUNK`].
struct Aligned<'m, M> {
    memory: &'m M,
    position: u64,
    /// Where the record ends, and how far `frontier` has been moved.
    end: u64,
    frontier: u64,
    stored: u64,
    word: [u8; 8],
    held: usize,
}

impl<'m, M: Stores> Aligned<'m, M> {
    /// The record from `start` to `end`, whose bytes are about to be pushed.
    fn new(memory: &'m M, start: u64, end: u64) -> Self {
        debug_assert_eq!(start % 8, 0);
        Self {
            memory,
            position: start,
            end,
            frontier: start,
            stored: 0,
            word: [0; 8],
            held: 0,
        }
    }

    fn push(&mut self, mut bytes: &[u8]) {
        self.stored += bytes.len() as u64;
        if self.held > 0 {
            let take = bytes.len().min(8 - self.held);
            self.word[self.held..self.held + take].copy_from_slice(&bytes[..take]);
            self.held += take;
            bytes = &bytes[take..];
            if self.held < 8 {
                return;
            }
            let word = self.word;
            self.emit(&word);
            self.held = 0;
        }
        let (whole, rest) = bytes.split_at(bytes.len() / 8 * 8);
        self.emit(whole);
        self.word[..rest.len()].copy_from_slice(rest);
        self.held = rest.len();
    }

    /// Store whole words at the position reached, each chunk after `frontier` passes it.
    fn emit(&mut self, mut words: &[u8]) {
        while !words.is_empty() {
            if self.position == self.frontier {
                self.frontier += (self.end - self.frontier).min(CHUNK as u64);
                // Before the chunk's bytes: a reader that copies one of them then loads this value.
                self.memory.store_frontier(self.frontier);
            }
            // Both on the grid of 8 and the gap at most a chunk: the piece is whole words.
            let take = words.len().min((self.frontier - self.position) as usize);
            let (piece, rest) = words.split_at(take);
            self.memory.store_ring(self.position, piece);
            self.position += take as u64;
            words = rest;
        }
    }

    /// Store the last word, if one is begun; the bytes pushed.
    fn finish(mut self) -> u64 {
        if self.held > 0 {
            self.word[self.held..].fill(0);
            let word = self.word;
            self.emit(&word);
        }
        self.stored
    }
}

/// Why a record was not written. Nothing in shared memory changed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// The record would be more than half the ring.
    TooLarge {
        /// The payload's length.
        bytes: usize,
        /// The longest payload this ring takes.
        max: usize,
    },
    /// The end record has been written.
    Ended,
    /// The positions are about to wrap, after 2^64 bytes.
    Spent,
}

/// The store after which a test stops a write, as a publisher that died there would have.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Never,
    AfterRegionEnd,
    AfterWrite,
    AfterLast,
}

/// The publisher's side of the ring. One exists per topic memory.
pub(crate) struct Writer<M> {
    memory: M,
    /// Bytes committed. The shared word is only stored, so what a peer does to it is not read.
    written: u64,
    /// The index of the next record.
    index: u32,
    /// Where each record still in the ring starts, oldest first.
    starts: VecDeque<u64>,
    ended: bool,
    #[cfg(test)]
    stop: Stop,
    /// Wakes sent: a test's count of the futex system calls this writer made.
    #[cfg(test)]
    wakes: u64,
}

impl<M: Stores> Writer<M> {
    /// A writer for memory nothing has been written to: every counter in it is zero.
    pub(crate) fn new(memory: M) -> Self {
        Self {
            memory,
            written: 0,
            index: 0,
            starts: VecDeque::new(),
            ended: false,
            #[cfg(test)]
            stop: Stop::Never,
            #[cfg(test)]
            wakes: 0,
        }
    }

    /// The longest payload one record carries.
    pub(crate) fn max_payload(&self) -> usize {
        let record = self.memory.capacity() / 2 / 8 * 8;
        usize::try_from(record.saturating_sub(rules::HEADER)).unwrap_or(usize::MAX)
    }

    /// Bytes in the ring.
    pub(crate) fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    /// Bytes committed so far: where the next record starts.
    pub(crate) fn position(&self) -> u64 {
        self.written
    }

    /// Where the next record ends if its payload is `len` bytes, or why it would be refused.
    pub(crate) fn end_of(&self, len: usize) -> Result<u64, Refused> {
        self.end_after(self.written, len)
    }

    /// How far every reader has to have read for the next record to leave none of them more than
    /// `window` records behind: where the record `window - 1` before the next starts, or 0 if the
    /// ring holds fewer.
    pub(crate) fn window_floor(&self, window: NonZeroUsize) -> u64 {
        match self.starts.len().checked_sub(window.get() - 1) {
            Some(at) => self.starts.get(at).copied().unwrap_or(self.written),
            None => 0,
        }
    }

    /// The index the next record gets.
    pub(crate) fn next_index(&self) -> u32 {
        self.index
    }

    /// Whether the end record has been written.
    pub(crate) fn ended(&self) -> bool {
        self.ended
    }

    /// Write one record, over the oldest ones when the ring is full, and commit it.
    pub(crate) fn put(&mut self, kind: Kind, payload: &[u8]) -> Result<(), Refused> {
        self.put_raw(kind as u32, payload.len(), [payload])
    }

    /// [`put`](Self::put) of the payload that is `parts` one after another, `len` bytes in all.
    pub(crate) fn put_parts<'p>(
        &mut self,
        kind: Kind,
        len: usize,
        parts: impl IntoIterator<Item = &'p [u8]>,
    ) -> Result<(), Refused> {
        self.put_raw(kind as u32, len, parts)
    }

    /// Tell the subscribers that sleep that something was committed; the `seq` that says so.
    pub(crate) fn announce(&self) -> u32 {
        self.memory.bump_seq()
    }

    /// Wake the subscribers that sleep on `seq`. The caller decides that one does (plan 15 §5.5).
    pub(crate) fn wake(&mut self) {
        self.memory.wake_seq();
        #[cfg(test)]
        {
            self.wakes += 1;
        }
    }

    /// The wakes sent so far.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn wakes(&self) -> u64 {
        self.wakes
    }

    /// Whether [`put`](Self::put) would write items with payloads of these lengths, one after
    /// another, without refusing any of them.
    pub(crate) fn admits(&self, lens: impl IntoIterator<Item = usize>) -> Result<(), Refused> {
        if self.ended {
            return Err(Refused::Ended);
        }
        let mut end = self.written;
        for len in lens {
            end = self.end_after(end, len)?;
        }
        Ok(())
    }

    /// Where a record with `len` bytes of payload that starts at `start` ends.
    fn end_after(&self, start: u64, len: usize) -> Result<u64, Refused> {
        if self.ended {
            return Err(Refused::Ended);
        }
        let max = self.max_payload();
        let Some(len) = u32::try_from(len).ok().filter(|_| len <= max) else {
            return Err(Refused::TooLarge { bytes: len, max });
        };
        start
            .checked_add(rules::record_size(len))
            .ok_or(Refused::Spent)
    }

    fn put_raw<'p>(
        &mut self,
        kind: u32,
        len: usize,
        parts: impl IntoIterator<Item = &'p [u8]>,
    ) -> Result<(), Refused> {
        let start = self.written;
        let end = self.end_after(start, len)?;
        // `end_after` bounded the length by half the ring.
        let len = len as u32;
        let capacity = self.memory.capacity();
        while self
            .starts
            .front()
            .is_some_and(|oldest| end - oldest > capacity)
        {
            self.starts.pop_front();
        }
        // Before `frontier` and the bytes: a reader that saw either then loads this value or newer.
        self.memory
            .store_tail(self.starts.front().copied().unwrap_or(start));
        self.memory.store_region_end(end);
        #[cfg(test)]
        if self.stop == Stop::AfterRegionEnd {
            return Ok(());
        }
        let header = Header {
            kind,
            len,
            index: self.index,
        };
        let mut words = Aligned::new(&self.memory, start, end);
        words.push(&header.bytes());
        for part in parts {
            words.push(part);
        }
        let stored = words.finish();
        debug_assert_eq!(
            stored,
            rules::HEADER + u64::from(len),
            "the parts are not `len` bytes"
        );
        self.memory.store_write(end);
        self.written = end;
        self.index = self.index.wrapping_add(1);
        self.starts.push_back(start);
        self.ended = kind == Kind::End as u32;
        #[cfg(test)]
        if self.stop == Stop::AfterWrite {
            return Ok(());
        }
        // After `write`: stored first, `last` would equal `write`, as on a topic without a record.
        self.memory.store_last(start);
        #[cfg(test)]
        if self.stop == Stop::AfterLast {
            return Ok(());
        }
        let floor = end.saturating_sub(capacity / 2);
        let oldest = self.starts.partition_point(|at| *at < floor);
        // The record just written starts at or above `floor`, so there is one.
        self.memory
            .store_half(self.starts.get(oldest).copied().unwrap_or(start));
        Ok(())
    }
}

/// Where a reader starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Start {
    /// At a position the publisher gave when it took the subscription: records from then on.
    Latest {
        /// The write counter at that moment.
        at: u64,
        /// The index of the record written there.
        index: u32,
    },
    /// At the newest record, so that the first receive returns the current value.
    Last,
    /// At the oldest record still in the ring.
    Oldest,
}

/// The publisher's memory failed a check. It has a defect or is hostile; the reader is done.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Corrupted(pub(crate) &'static str);

/// What a receive call found.
#[derive(Debug)]
pub(crate) enum Taken<'a> {
    /// The next item. The reader has moved past it.
    Item {
        /// The record's index.
        #[cfg_attr(
            not(any(test, feature = "fuzzing")),
            expect(
                dead_code,
                reason = "a subscriber counts losses itself; tests check the index"
            )
        )]
        index: u32,
        /// Its payload, in the reader's buffer until the next call, which a decoder may take.
        record: Record<'a>,
    },
    /// The topic's end record. The reader stays on it: every later call returns it again.
    End {
        /// Its payload.
        payload: &'a [u8],
    },
    /// The next value is in a slot of the pool. The reader has moved past the record.
    Loan {
        /// The record's index.
        #[cfg_attr(
            not(any(test, feature = "fuzzing")),
            expect(
                dead_code,
                reason = "a subscriber counts losses itself; tests check the index"
            )
        )]
        index: u32,
        loan: Loan,
    },
    /// No committed record is unread; or a reader [stopped at](Reader::stop_at) an end stepped
    /// over a record of a kind it skips to that end, and records after it are unread; or the call
    /// gave up, which [`Reader::gave_up`] tells, with its causes.
    Empty,
    /// This many records were overwritten unread. The next call returns the one after them.
    Lagged(u64),
}

/// [`Taken`] before it borrows the buffer.
enum Outcome {
    Item { index: u32, len: usize },
    End { len: usize },
    Loan { index: u32, loan: Loan },
    Empty,
    Lagged(u64),
}

/// A header loaded at `at` whose bytes a judgment by `frontier` found not written over.
#[derive(Clone, Copy, Debug)]
struct Judged {
    at: u64,
    header: Header,
    size: u64,
}

/// What a copy of a record found.
enum Copied {
    /// Every piece judged whole: the header and the payload are the record's.
    Whole(Judged),
    /// A piece may hold bytes of a newer record, the header's too.
    Overtaken,
}

#[cfg(all(test, not(loom)))]
thread_local! {
    /// Set by a test: run before a judgment loads `frontier`, as a publisher writing then would.
    pub(crate) static BEFORE_JUDGING: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}

/// A subscriber's side of the ring. Any number read one topic memory, each at its own pace.
pub(crate) struct Reader<M> {
    memory: M,
    /// Where the next record to return starts, and the record owed; not in shared memory.
    cursor: Cursor,
    seen: Seen,
    /// Set by the first check that fails; every later call fails with it.
    failed: Option<&'static str>,
    /// Records reported as lost so far.
    lost: u64,
    /// Whether the last call stopped short of `write`, as [`Reader::gave_up`] lists.
    gave_up: bool,
    /// The payload of the record last returned, padded to 8.
    payload: Vec<u8>,
}

impl<M: Loads> Reader<M> {
    /// Start reading topic memory at `start`. Nothing is stored anywhere.
    pub(crate) fn attach(memory: M, start: Start) -> Result<Self, Corrupted> {
        let mut reader = Self {
            memory,
            cursor: Cursor::attached(0, None),
            seen: Seen::default(),
            failed: None,
            lost: 0,
            gave_up: false,
            payload: Vec::new(),
        };
        let view = reader.view()?;
        reader.cursor = match start {
            Start::Latest { at, index } => {
                if at % 8 != 0 || at > view.write {
                    return Err(Corrupted("start not a position the publisher committed"));
                }
                Cursor::attached(at, Some(index))
            }
            // No record yet: `write` is zero, and so is the place the first one goes.
            Start::Last if view.write == 0 => Cursor::attached(0, None),
            Start::Last => Cursor::attached(reader.newest()?, None),
            Start::Oldest => {
                // That `tail` was loaded after `write` and may be above it; a later `write` is not.
                if view.tail > view.write {
                    reader.reload()?;
                }
                Cursor::attached(view.tail, None)
            }
        };
        Ok(reader)
    }

    /// Where the next record to return starts.
    pub(crate) fn position(&self) -> u64 {
        self.cursor.read()
    }

    /// Records reported as [`Taken::Lagged`], or counted by
    /// [`count_lost_before`](Self::count_lost_before), so far.
    pub(crate) fn lost(&self) -> u64 {
        self.lost
    }

    /// Whether the last receive call gave up before it reached `write`: lapped again after
    /// [`RETRIES`] resumes, past more records of kinds it skips than the ring holds, or, in
    /// [`try_latest`](Self::try_latest), lapped or past a record of a kind it skips once more
    /// after [`RETRIES`] of them, or, [stopped at](Self::stop_at) an end, lapped where a lap would
    /// resume at that end or past it, which may be at the first lap. Its result was
    /// [`Taken::Empty`] or [`Taken::Lagged`], though [`pending`](Self::pending) may hold.
    pub(crate) fn gave_up(&self) -> bool {
        self.gave_up
    }

    /// Whether the publisher committed past this reader: one `Acquire` load, as a wait needs.
    pub(crate) fn pending(&self) -> bool {
        self.memory.write() != self.cursor.read()
    }

    /// Where the publisher's committed records end now.
    pub(crate) fn committed(&self) -> u64 {
        self.memory.write()
    }

    /// The first `len` bytes of the buffer the last item was copied into.
    pub(crate) fn held(&self, len: usize) -> &[u8] {
        &self.payload[..len]
    }

    /// Stop every later [`try_recv`](Self::try_recv) short of `end`, a record start the publisher
    /// committed; with `None`, read on. No move of a `try_recv` from before `end` reaches it or
    /// passes it but a step over the record just before it. A reader already at `end` is not held
    /// there: the caller stops calling `try_recv` once [`position`](Self::position) reaches it.
    /// One that steps over records of kinds it skips to `end` returns there: [`Taken::Empty`], or
    /// [`Taken::Lagged`] for records missed among them. A lapped one resumes at `tail`, not
    /// `half`, until two laps in a row overtake the record a resume at `tail` put it on before it
    /// moved past it, and at `half` from then on (`cursor.rs` says why); where a resume would be
    /// `end` or past it, it gives up instead, so that
    /// [`count_lost_before`](Self::count_lost_before) counts what it missed. Each call with
    /// `Some` starts a drain that resumes at `tail` first.
    /// A reader stopped at `end` reaches it only having returned or counted every record before
    /// it, but for those before the first record read. [`try_latest`](Self::try_latest) does not
    /// look at `end`, and may move past it.
    pub(crate) fn stop_at(&mut self, end: Option<u64>) {
        self.cursor.stop_at(end);
    }

    /// The next record, without waiting.
    pub(crate) fn try_recv(&mut self) -> Result<Taken<'_>, Corrupted> {
        let outcome = self.next()?;
        Ok(self.lend(outcome))
    }

    /// The newest record, without waiting; what lies before it is skipped and not counted as lost.
    pub(crate) fn try_latest(&mut self) -> Result<Taken<'_>, Corrupted> {
        let outcome = self.latest()?;
        Ok(self.lend(outcome))
    }

    /// Count as lost the records before the end this reader is [stopped at](Self::stop_at),
    /// a record start the publisher had committed, that it has neither returned nor counted: how
    /// many. A drain stops there after a [`try_recv`](Self::try_recv) that [gave
    /// up](Self::gave_up), or after calls that left it where it was, two in a row of which only
    /// a publisher that breaks the protocol makes.
    ///
    /// The reader knows the first record it owes: the one at its position, or, after a lap, one
    /// at or before it, never at the end or past it, as no move reaches the end but a step over
    /// a record judged whole. So with the reader at the end, or not stopped at one, or before the
    /// first record read, nothing is counted. Otherwise the count is exact when the record at the
    /// end is committed and judged whole: its index less the one owed, less what an earlier lower
    /// bound counted of them; the reader then moves to the end. Else it is one, the record owed,
    /// unless a lower bound counted it already, and then none: a lower bound. Either way the
    /// losses reported never count a record at the end or after it, and add up to the records
    /// before it not returned once the reader goes on past it.
    pub(crate) fn count_lost_before(&mut self) -> Result<u64, Corrupted> {
        self.failed()?;
        let Some(end) = self.cursor.owing_before_end() else {
            return Ok(0);
        };
        let lost = match self.index_at(end)? {
            Some(judged) => self.cursor.count_to_end(judged),
            None => self.cursor.credit(),
        };
        self.lost = self.lost.saturating_add(lost);
        Ok(lost)
    }

    fn lend(&mut self, outcome: Outcome) -> Taken<'_> {
        match outcome {
            Outcome::Item { index, len } => Taken::Item {
                index,
                record: Record::new(&mut self.payload, len),
            },
            Outcome::End { len } => Taken::End {
                payload: &self.payload[..len],
            },
            Outcome::Loan { index, loan } => Taken::Loan { index, loan },
            Outcome::Empty => Taken::Empty,
            Outcome::Lagged(records) => Taken::Lagged(records),
        }
    }

    /// Remember a failed check, so that the reader stays failed.
    fn keep<T>(&mut self, checked: Result<T, Corrupted>) -> Result<T, Corrupted> {
        if let Err(Corrupted(what)) = checked {
            self.failed = Some(what);
        }
        checked
    }

    /// The failure every call after the first one returns.
    fn failed(&self) -> Result<(), Corrupted> {
        match self.failed {
            Some(what) => Err(Corrupted(what)),
            None => Ok(()),
        }
    }

    /// Where an attach starts: `tail` on both sides of `write`, as each bound needs its own order.
    fn view(&mut self) -> Result<View, Corrupted> {
        let before = self.memory.tail();
        let write = self.memory.write();
        let tail = self.memory.tail();
        let view = self.seen.view(before, write, tail, self.memory.capacity());
        self.keep(view)
    }

    /// Load `write` again; the reader is at the one it held.
    fn reload(&mut self) -> Result<u64, Corrupted> {
        let write = self.memory.write();
        let newer = self.seen.newer(write);
        self.keep(newer)?;
        Ok(write)
    }

    /// Whether `frontier`, loaded after a copy from `from`, says those bytes may be written over.
    fn overwritten(&mut self, from: u64) -> Result<bool, Corrupted> {
        #[cfg(all(test, not(loom)))]
        BEFORE_JUDGING.with_borrow_mut(|hook| hook.iter_mut().for_each(|hook| hook()));
        self.memory.fence();
        let frontier = self.memory.frontier();
        let frontier = self.seen.frontier(frontier);
        Ok(self.keep(frontier)? > from.saturating_add(self.memory.capacity()))
    }

    /// The start of the newest record. The `write` held says one is committed.
    fn newest(&mut self) -> Result<u64, Corrupted> {
        let tail = self.memory.tail();
        let tail = self.seen.tail(tail, self.memory.capacity());
        self.keep(tail)?;
        let last = self.memory.last();
        let write = self.memory.write();
        let last = self.seen.kept(last, write, "last outside the kept records");
        self.keep(last)
    }

    /// Resume a lapped reader as the cursor says; `false` after [`RETRIES`] or at a drain's end.
    fn lap(&mut self, laps: &mut u32) -> Result<bool, Corrupted> {
        // Every overtaken copy or judgment is a lap the cursor hears of, the one that gives up too.
        let at_tail = self.cursor.lapped();
        if *laps == RETRIES {
            return Ok(false);
        }
        *laps += 1;
        // After the `frontier` that found the lap: at least the `tail` stored before it.
        let tail = self.memory.tail();
        let tail = self.seen.lapped_tail(tail, self.memory.capacity());
        let tail = self.keep(tail)?;
        let (start, what) = if at_tail {
            (tail, "tail at the write counter")
        } else {
            (self.memory.half(), "half outside the kept records")
        };
        let write = self.memory.write();
        let start = self.seen.kept(start, write, what);
        let start = self.keep(start)?;
        Ok(self.cursor.jump(start))
    }

    fn header(&self, position: u64) -> Header {
        let mut bytes = [0; 8];
        self.memory.load_ring(position, &mut bytes);
        Header::parse(bytes)
    }

    /// `header`, loaded at `at`, once `frontier` says it was not written over; `None` if it may be.
    fn judge(&mut self, at: u64, header: Header) -> Result<Option<Judged>, Corrupted> {
        let size = header.extent(at, self.seen.write(), self.memory.capacity());
        if self.overwritten(at)? {
            return Ok(None);
        }
        let size = self.keep(size)?;
        Ok(Some(Judged { at, header, size }))
    }

    /// The record at `at`, if it is committed and judged whole.
    fn index_at(&mut self, at: u64) -> Result<Option<Judged>, Corrupted> {
        if at % 8 != 0 || (at >= self.seen.write() && self.reload()? <= at) {
            return Ok(None);
        }
        let header = self.header(at);
        let size = header.extent(at, self.seen.write(), self.memory.capacity());
        // Believed only once `frontier` says the publisher was not writing over it.
        let whole = !self.overwritten(at)?;
        Ok(size
            .ok()
            .filter(|_| whole)
            .map(|size| Judged { at, header, size }))
    }

    /// Copy the payload of `header`'s record at `position` by [`CHUNK`]s, each judged.
    fn copy(&mut self, position: u64, header: Header, size: u64) -> Result<Copied, Corrupted> {
        // `Header::extent` bounded `size` by half the ring, which is mapped: it fits a `usize`.
        let len = (size - rules::HEADER) as usize;
        if self.payload.capacity() < len {
            // A taken or outgrown buffer: a new one, as a `reserve` would copy the old bytes over.
            self.payload = Vec::with_capacity(len);
        } else {
            self.payload.clear();
        }
        let (mut copied, mut from) = (0, position);
        loop {
            let end = len.min(copied + CHUNK);
            let at = position + rules::HEADER + copied as u64;
            self.memory
                .load_ring_extend(at, end - copied, &mut self.payload);
            if self.overwritten(from)? {
                return Ok(Copied::Overtaken);
            }
            if end == len {
                let judged = Judged {
                    at: position,
                    header,
                    size,
                };
                return Ok(Copied::Whole(judged));
            }
            (copied, from) = (end, at + (end - copied) as u64);
        }
    }

    /// Count what `lost` holds as reported.
    fn settle(&mut self, lost: u64) -> Outcome {
        if lost == 0 {
            return Outcome::Empty;
        }
        self.lost = self.lost.saturating_add(lost);
        Outcome::Lagged(lost)
    }

    /// [`settle`](Self::settle) for a call that stops short of `write`.
    fn give_up(&mut self, lost: u64) -> Outcome {
        self.gave_up = true;
        self.settle(lost)
    }

    fn next(&mut self) -> Result<Outcome, Corrupted> {
        self.gave_up = false;
        self.failed()?;
        let capacity = self.memory.capacity();
        let (mut lost, mut laps) = (0u64, 0u32);
        // A ring holds at most this many records, so no more than that are stepped over.
        let mut steps = capacity / rules::HEADER;
        loop {
            let read = self.cursor.read();
            // Below the `write` held, every record is committed: it is loaded only once reached.
            if read == self.seen.write() && self.reload()? == read {
                return Ok(self.settle(lost));
            }
            let header = self.header(read);
            let size = match header.extent(read, self.seen.write(), capacity) {
                Ok(size) if self.cursor.expects(header) && header.kind <= Kind::Loan as u32 => size,
                _ => {
                    // Believed only once `frontier` says the publisher was not writing over it.
                    let Some(judged) = self.judge(read, header)? else {
                        if !self.lap(&mut laps)? {
                            return Ok(self.give_up(lost));
                        }
                        continue;
                    };
                    lost += self.cursor.count_at(&judged);
                    if header.kind > Kind::Loan as u32 {
                        // A drain stepped to its end has read all it had: the rest is not its own.
                        if self.cursor.step(judged) {
                            return Ok(self.settle(lost));
                        }
                        if steps == 0 {
                            return Ok(self.give_up(lost));
                        }
                        steps -= 1;
                        continue;
                    }
                    judged.size
                }
            };
            if lost > 0 {
                // The reader stays on this record: the next call returns it or finds it gone.
                return Ok(self.settle(lost));
            }
            // Only once copied and judged are the header and payload known to be this record's.
            let judged = match self.copy(read, header, size)? {
                Copied::Whole(judged) => judged,
                // A lap at `read`: nothing is counted or stepped over by a header that may be new.
                Copied::Overtaken => {
                    if !self.lap(&mut laps)? {
                        return Ok(self.give_up(lost));
                    }
                    continue;
                }
            };
            let len = header.len as usize;
            if header.kind == Kind::End as u32 {
                self.cursor.rest_on(judged);
                return Ok(Outcome::End { len });
            }
            let outcome = self.outcome(header)?;
            self.cursor.step(judged);
            return Ok(outcome);
        }
    }

    /// An item or a loan, from the payload just copied and judged.
    fn outcome(&mut self, header: Header) -> Result<Outcome, Corrupted> {
        let len = header.len as usize;
        if header.kind != Kind::Loan as u32 {
            return Ok(Outcome::Item {
                index: header.index,
                len,
            });
        }
        let loan =
            Loan::parse(&self.payload[..len]).ok_or(Corrupted("loan record of another length"));
        let loan = self.keep(loan)?;
        Ok(Outcome::Loan {
            index: header.index,
            loan,
        })
    }

    fn latest(&mut self) -> Result<Outcome, Corrupted> {
        self.gave_up = false;
        self.failed()?;
        let capacity = self.memory.capacity();
        let mut laps = 0u32;
        loop {
            let read = self.cursor.read();
            if self.reload()? == read {
                return Ok(Outcome::Empty);
            }
            let last = self.newest()?;
            // Above `last` only when the publisher has stored `write` and not `last` yet.
            let at = read.max(last);
            let header = self.header(at);
            // `at` is below the `write` held either way, and the record there is committed.
            let judged = match header.extent(at, self.seen.write(), capacity) {
                Ok(size) if header.kind <= Kind::Loan as u32 => {
                    match self.copy(at, header, size)? {
                        Copied::Whole(judged) => Some(judged),
                        Copied::Overtaken => None,
                    }
                }
                _ => self.judge(at, header)?,
            };
            let Some(judged) = judged else {
                if laps == RETRIES {
                    return Ok(self.give_up(0));
                }
                laps += 1;
                continue;
            };
            if header.kind > Kind::Loan as u32 {
                self.cursor.skip_to(judged);
                if laps == RETRIES {
                    return Ok(self.give_up(0));
                }
                laps += 1;
                continue;
            }
            let len = header.len as usize;
            if header.kind == Kind::End as u32 {
                self.cursor.rest_on(judged);
                return Ok(Outcome::End { len });
            }
            let outcome = self.outcome(header)?;
            self.cursor.skip_to(judged);
            return Ok(outcome);
        }
    }
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
mod tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod mapped_tests;

#[cfg(all(test, loom))]
mod loom_tests;

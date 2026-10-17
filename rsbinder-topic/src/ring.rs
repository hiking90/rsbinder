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
//! Kind 0 is an item and kind 1 the end of the topic, after which the publisher writes nothing.
//! Kinds 2 and 3 belong to later versions of the layout (plan 15 gives 2 to a loaned slot and
//! keeps 3 free); a reader of this version steps over them by their length. One record is at most
//! half the capacity.
//!
//! # The publisher
//!
//! There is one. It never waits and never reads the shared words; what it needs to know it keeps
//! to itself (the positions of the records still in the ring). To write a record of `r` bytes at
//! position `w` it stores, in this order:
//!
//! 1. `tail`: the start of the oldest record that survives, so that `w + r - tail <= capacity`.
//!    From here on a reader treats everything below `tail` as gone.
//! 2. `region_end = w + r`, the word a libfmq reader of the unsynchronized flavor judges by.
//! 3. The record's bytes, with `Release` stores.
//! 4. `write = w + r`. The record is committed.
//! 5. `last = w`, the start of the newest record.
//! 6. `half`: the start of the oldest record at or above `write - capacity / 2`.
//!
//! Between any two of these stores `tail <= half <= last < write` and `write - tail <= capacity`
//! hold, so a publisher that dies there leaves memory a reader can finish reading.
//!
//! # A subscriber
//!
//! It keeps its own read position, in its own memory, and only loads the shared words. A record
//! is copied out, header and payload, and judged afterwards: an acquire fence, then one load of
//! `tail`, and if `tail` has passed the record's start, what was copied may be bytes of a newer
//! record and is dropped. That check is ordered, not merely likely to work: the publisher's
//! stores are `Release`, so a reader that copied even one byte of an overwriting record then
//! loads a `tail` at least as new as the one stored before that record. A header is believed
//! before that check only when it is what the reader expects (the next index, a length that fits
//! what is committed); any other header is judged by `tail` first, so a torn one is not taken
//! for corruption.
//!
//! The reader holds the newest `write` it loaded and loads it again only once it has read up to
//! it: every record below it is committed, and the acquire after that load orders the copies of
//! them. A received record thus costs one counter load. The checks on the counters compare each
//! with the reader's own state, or two loaded in the order that makes the bound hold for a
//! publisher that follows the protocol: `write - tail <= capacity` with `write` loaded first,
//! `tail <= write` with `tail` first (only after a lap, where `half` is checked against both).
//!
//! A reader the publisher has lapped resumes at `half`. Resuming at `tail` would put it on the
//! record the next publish overwrites; from `half` the publisher has to write half a ring before
//! it catches up again. How many records were skipped is the distance between the index the
//! reader expected and the index of the first record it reads after the jump, so the losses it
//! reports add up to the records it did not return.
//!
//! The publisher's memory is not trusted. Lengths and counters are checked before use
//! ([`Corrupted`]); the place a copy starts is a position modulo the capacity and its length is
//! bounded by half the capacity, so no value in shared memory makes a copy leave the ring; and a
//! call that finds itself lapped [`RETRIES`] times gives up, so a publisher that keeps moving
//! `tail` cannot hold a caller.

use std::collections::VecDeque;

mod rules;

use rules::{Header, Seen, View};

use crate::Record;

/// How often one receive call resumes after being lapped before it returns without a record.
pub(crate) const RETRIES: u32 = 4;

/// The kinds of record this version writes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// One published value.
    Item = 0,
    /// The topic's last record; its payload says how it ended.
    End = 1,
}

/// Topic memory as the publisher reaches it. Each store is `Release`.
pub(crate) trait Stores {
    /// Bytes in the ring: a multiple of 8, never zero.
    fn capacity(&self) -> u64;
    fn store_tail(&self, position: u64);
    fn store_region_end(&self, position: u64);
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
    fn write(&self) -> u64;
    fn last(&self) -> u64;
    fn half(&self) -> u64;
    /// Fill `dst` from `position`, wrapping at the ring's end, with `Relaxed` loads. Both are
    /// multiples of 8.
    fn load_ring(&self, position: u64, dst: &mut [u8]);
    /// An acquire fence: the loads that follow come after the stores the ring's loads saw.
    fn fence(&self);
}

/// Bytes of any lengths stored one after another from `position`, in the whole words
/// [`Stores::store_ring`] takes: a word that two parts share is stored once both gave it bytes,
/// and the last one is padded with zeros.
struct Aligned<'m, M> {
    memory: &'m M,
    position: u64,
    stored: u64,
    word: [u8; 8],
    held: usize,
}

impl<'m, M: Stores> Aligned<'m, M> {
    fn new(memory: &'m M, position: u64) -> Self {
        debug_assert_eq!(position % 8, 0);
        Self {
            memory,
            position,
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
            self.memory.store_ring(self.position, &self.word);
            self.position += 8;
            self.held = 0;
        }
        let (whole, rest) = bytes.split_at(bytes.len() / 8 * 8);
        if !whole.is_empty() {
            self.memory.store_ring(self.position, whole);
            self.position += whole.len() as u64;
        }
        self.word[..rest.len()].copy_from_slice(rest);
        self.held = rest.len();
    }

    /// Store the last word, if one is begun; the bytes pushed.
    fn finish(mut self) -> u64 {
        if self.held > 0 {
            self.word[self.held..].fill(0);
            self.memory.store_ring(self.position, &self.word);
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
        // Before any byte of the record: a reader that copies one of them then loads this value.
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
        self.memory.store_ring(start, &header.bytes());
        let mut words = Aligned::new(&self.memory, start + rules::HEADER);
        for part in parts {
            words.push(part);
        }
        let stored = words.finish();
        debug_assert_eq!(stored, u64::from(len), "the parts are not `len` bytes");
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
    /// No committed record is unread, or the call was lapped [`RETRIES`] times and gave up.
    Empty,
    /// This many records were overwritten unread. The next call returns the one after them.
    Lagged(u64),
}

/// [`Taken`] before it borrows the buffer.
enum Outcome {
    Item { index: u32, len: usize },
    End { len: usize },
    Empty,
    Lagged(u64),
}

/// A subscriber's side of the ring. Any number read one topic memory, each at its own pace.
pub(crate) struct Reader<M> {
    memory: M,
    /// Where the next record to return starts. It lives here, not in shared memory.
    read: u64,
    /// The index that record has if none was lost; `None` until a first record sets it.
    expected: Option<u32>,
    seen: Seen,
    /// Set by the first check that fails; every later call fails with it.
    failed: Option<&'static str>,
    /// Records reported as lost so far.
    lost: u64,
    /// The payload of the record last returned, padded to 8.
    payload: Vec<u8>,
}

impl<M: Loads> Reader<M> {
    /// Start reading topic memory at `start`. Nothing is stored anywhere.
    pub(crate) fn attach(memory: M, start: Start) -> Result<Self, Corrupted> {
        let mut reader = Self {
            memory,
            read: 0,
            expected: None,
            seen: Seen::default(),
            failed: None,
            lost: 0,
            payload: Vec::new(),
        };
        let view = reader.view()?;
        match start {
            Start::Latest { at, index } => {
                if at % 8 != 0 || at > view.write {
                    return Err(Corrupted("start not a position the publisher committed"));
                }
                reader.read = at;
                reader.expected = Some(index);
            }
            // No record yet: `write` is zero, and so is the place the first one goes.
            Start::Last if view.write == 0 => {}
            Start::Last => reader.read = reader.newest()?,
            Start::Oldest => {
                reader.read = view.tail;
                // That `tail` was loaded after `write` and may be above it; a later `write` is not.
                if view.tail > view.write {
                    reader.reload()?;
                }
            }
        }
        Ok(reader)
    }

    /// Where the next record to return starts.
    pub(crate) fn position(&self) -> u64 {
        self.read
    }

    /// Records reported as [`Taken::Lagged`] so far.
    pub(crate) fn lost(&self) -> u64 {
        self.lost
    }

    /// Whether the publisher committed past this reader: one `Acquire` load, as a wait needs.
    pub(crate) fn pending(&self) -> bool {
        self.memory.write() != self.read
    }

    /// Where the publisher's committed records end now.
    pub(crate) fn committed(&self) -> u64 {
        self.memory.write()
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

    fn lend(&mut self, outcome: Outcome) -> Taken<'_> {
        match outcome {
            Outcome::Item { index, len } => Taken::Item {
                index,
                record: Record::new(&mut self.payload, len),
            },
            Outcome::End { len } => Taken::End {
                payload: &self.payload[..len],
            },
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

    /// Whether the publisher has begun to write over `position`, judged after what was copied.
    fn overwritten(&mut self, position: u64) -> Result<bool, Corrupted> {
        self.memory.fence();
        let tail = self.memory.tail();
        let tail = self.seen.tail(tail, self.memory.capacity());
        Ok(position < self.keep(tail)?)
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

    /// Move a lapped reader to `half`; `false` once this call has done so [`RETRIES`] times.
    /// `overwritten` has just loaded `tail`.
    fn resume(&mut self, laps: &mut u32) -> Result<bool, Corrupted> {
        if *laps == RETRIES {
            return Ok(false);
        }
        *laps += 1;
        let half = self.memory.half();
        let write = self.memory.write();
        let half = self.seen.kept(half, write, "half outside the kept records");
        self.read = self.keep(half)?;
        Ok(true)
    }

    fn header(&self, position: u64) -> Header {
        let mut bytes = [0; 8];
        self.memory.load_ring(position, &mut bytes);
        Header::parse(bytes)
    }

    /// Copy the payload of the record at `position`, `size` bytes long, into the buffer.
    fn copy(&mut self, position: u64, size: u64) {
        // `Header::extent` bounded `size` by half the ring, which is mapped: it fits a `usize`.
        let len = (size - rules::HEADER) as usize;
        if self.payload.capacity() < len {
            // A taken or outgrown buffer: zeroed allocation, so a large one is not filled first.
            self.payload = vec![0; len];
        } else {
            self.payload.resize(len, 0);
        }
        self.memory
            .load_ring(position + rules::HEADER, &mut self.payload);
    }

    /// Count what `lost` holds as reported.
    fn settle(&mut self, lost: u64) -> Outcome {
        if lost == 0 {
            return Outcome::Empty;
        }
        self.lost = self.lost.saturating_add(lost);
        Outcome::Lagged(lost)
    }

    fn next(&mut self) -> Result<Outcome, Corrupted> {
        self.failed()?;
        let capacity = self.memory.capacity();
        let (mut lost, mut laps) = (0u64, 0u32);
        // A ring holds at most this many records, so no more than that are stepped over.
        let mut steps = capacity / rules::HEADER;
        loop {
            // Below the `write` held, every record is committed: it is loaded only once reached.
            if self.read == self.seen.write() && self.reload()? == self.read {
                return Ok(self.settle(lost));
            }
            let header = self.header(self.read);
            let expected = self.expected.is_none_or(|index| index == header.index);
            let size = match header.extent(self.read, self.seen.write(), capacity) {
                Ok(size) if expected && header.kind <= Kind::End as u32 => size,
                size => {
                    // Believed only once `tail` says the publisher was not writing over it.
                    if self.overwritten(self.read)? {
                        if !self.resume(&mut laps)? {
                            return Ok(self.settle(lost));
                        }
                        continue;
                    }
                    let size = self.keep(size)?;
                    if let Some(expected) = self.expected {
                        lost += u64::from(header.index.wrapping_sub(expected));
                    }
                    if header.kind > Kind::End as u32 {
                        self.read += size;
                        self.expected = Some(header.index.wrapping_add(1));
                        if steps == 0 {
                            return Ok(self.settle(lost));
                        }
                        steps -= 1;
                        continue;
                    }
                    // The reader stays on this record: the next call returns it or finds it gone.
                    self.expected = Some(header.index);
                    return Ok(self.settle(lost));
                }
            };
            if lost > 0 {
                // Losses counted from records stepped over; this one is read by the next call.
                return Ok(self.settle(lost));
            }
            self.copy(self.read, size);
            // Only now is what was copied, header and payload, known to be this record's.
            if self.overwritten(self.read)? {
                // A record the reader has not read yet does not count before its first.
                if let Some(index) = self.expected {
                    lost = 1;
                    self.expected = Some(index.wrapping_add(1));
                }
                if !self.resume(&mut laps)? {
                    return Ok(self.settle(lost));
                }
                continue;
            }
            let len = header.len as usize;
            if header.kind == Kind::End as u32 {
                self.expected = Some(header.index);
                return Ok(Outcome::End { len });
            }
            self.read += size;
            self.expected = Some(header.index.wrapping_add(1));
            return Ok(Outcome::Item {
                index: header.index,
                len,
            });
        }
    }

    fn latest(&mut self) -> Result<Outcome, Corrupted> {
        self.failed()?;
        let capacity = self.memory.capacity();
        let mut laps = 0u32;
        loop {
            if self.reload()? == self.read {
                return Ok(Outcome::Empty);
            }
            let last = self.newest()?;
            // Above `last` only when the publisher has stored `write` and not `last` yet.
            let at = self.read.max(last);
            let header = self.header(at);
            // `at` is below the `write` held either way, and the record there is committed.
            let size = header.extent(at, self.seen.write(), capacity);
            match size {
                Ok(size) if header.kind <= Kind::End as u32 => self.copy(at, size),
                _ => {}
            }
            if self.overwritten(at)? {
                if laps == RETRIES {
                    return Ok(Outcome::Empty);
                }
                laps += 1;
                continue;
            }
            let size = self.keep(size)?;
            if header.kind > Kind::End as u32 {
                self.read = at + size;
                self.expected = Some(header.index.wrapping_add(1));
                if laps == RETRIES {
                    return Ok(Outcome::Empty);
                }
                laps += 1;
                continue;
            }
            let len = header.len as usize;
            if header.kind == Kind::End as u32 {
                self.read = at;
                self.expected = Some(header.index);
                return Ok(Outcome::End { len });
            }
            self.read = at + size;
            self.expected = Some(header.index.wrapping_add(1));
            return Ok(Outcome::Item {
                index: header.index,
                len,
            });
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

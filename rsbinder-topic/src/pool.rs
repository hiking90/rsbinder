// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loans (plan 15 §6): the slots of a topic's pool, which the publisher lends out and fills, and
//! the pin a subscriber holds on one while it reads it.
//!
//! # The pool
//!
//! A topic made with [`PoolPolicy`](crate::PoolPolicy) has a second memfd besides topic memory:
//! a fixed number of slots of a fixed size, and one 64-bit generation per slot (`layout.rs` has
//! the bytes). Only the publisher writes it; subscribers map it read-only. A loaned value is
//! written into a slot, and the ring carries a `LOAN` record that names the slot, the length and
//! the generation the slot had when it was published.
//!
//! # Generations
//!
//! Even: the slot holds the value its `LOAN` record of that generation announced. Odd: the
//! publisher is writing the slot, or has taken it back and will. Only the publisher stores a
//! generation, and each store adds one, so a value never comes back.
//!
//! # Pins (§6.2)
//!
//! A subscriber's control page has one pin word ([`MAX_PINS`]): 0, or a slot's number plus one.
//!
//! ```text
//! subscriber, holding a LOAN record {slot i, generation g}:
//!   pin = i + 1 (Release); fence(SeqCst); generation[i] == g ? read the slot : pin = 0, lose it
//!   done reading: pin = 0 (Release)
//! publisher, to lend slot i:
//!   generation[i] = odd; fence(SeqCst); load every pin word
//!   the first slot tried, i or one before it, that no pin word names: lend it; none: try another
//!   lending it: fence(Acquire), then the slot is written
//!   published: generation[i] = even (Release), then the LOAN record, then the values past the
//!   newest `depth` taken back: generation = odd (Release)
//! ```
//!
//! The pin word is the subscriber's memory and the generations the publisher's, each read-only
//! to the other side, where an atomic load is `Relaxed` alone: the `SeqCst` fences order each
//! side's store before its load. Of a subscriber that pins as the publisher takes the slot back,
//! one of the two sees the other's store: the subscriber finds the odd generation, or the
//! publisher finds the pin. Every store of the pin word is `Release`, 0 and a slot's number
//! alike, so whichever the publisher loads, its acquire fence after the load orders the
//! subscriber's reads of the slots it pinned before that store before the writes the publisher
//! then makes. A `Relaxed` store of the next pin would not do: it is no part of the release
//! sequence of the 0 before it, and a publisher that loads it lends the slot just let go.
//!
//! A subscriber taken off the publisher's list has its pin word loaded no more, so no lend's
//! fence follows a load of its stores. [`take_ended`] fences instead, after the `Relaxed` load
//! that found it ended: the `Release` store that ended it comes after its last read of the pool,
//! and whichever thread lends next comes after the one that took it off.
//!
//! Every slot tried stays odd, so a later fence and load of the pin words decide it as well as
//! its own did. The publisher judges the slots tried so far by one load of all the pin words, not
//! each by its own load: a subscriber that moves its pin from slot to slot during one lend is
//! then seen on one slot only, and with more slots tried than there are pin words one is free.
//!
//! # Which slot is lent
//!
//! A slot that every subscriber has read past (the `read` its control page reports is at or above
//! the end of the slot's `LOAN` record, or the slot has held no value, which counts as published
//! before every other) is needed by none of them; of those, the one published
//! second last is lent first, and the one published last only when it is the only one read past.
//! The slots published last are still in the shared cache, so that a pool larger than it keeps
//! reusing them (plan 15 §8.4). The one published last is the one its subscribers read last,
//! and their cores still hold its lines: a write must first take them back from each, which on
//! an x86_64 desktop made a 1 MiB fill 3.3 times as slow (plan 15). Passing it over saves at
//! most what a subscriber core's private cache holds of it, and only for a subscriber that reads
//! the value whole. It costs once the two slots written in turn, with the buffers copied from and
//! into, outgrow the last-level cache: from a slot of 3 to 6 MiB upward on a 16 MiB one, by how
//! the value is read, where the slot read last would have been the one still cached (plan 15).
//! With none read past, the oldest is lent, which keeps the newest values longest; a lossless
//! pool (below) waits instead.
//! A `read` is the subscriber's to write: a wrong one only changes which slot is lent, and the
//! generation check still decides what a subscriber reads, so it costs that subscriber values.
//!
//! # How far behind a subscriber may fall
//!
//! Each publish takes back the values older than the newest `depth`: their generation goes odd,
//! and a subscriber that reaches their `LOAN` records loses them, without a copy. A subscriber
//! more than `depth` values behind thus skips to the newest `depth`, as one behind the ring
//! skips to what the ring still holds. The slots kept for pins would otherwise hold older values
//! whenever no pin is on them, and a subscriber that cannot keep up would read values as old as
//! the pool is deep (plan 15 §8.4).
//!
//! A publish takes values back only after its own `LOAN` record is committed, so a value is taken
//! back only once a newer one is readable, and with a depth of 1 the newest record's slot stays
//! even until a later record is committed. The odd generation is stored `Release`: a subscriber
//! whose pin finds it then loads a `write` at or past that newer record, which is how the newest
//! value's reader ([`Subscriber::try_latest`](crate::Subscriber::try_latest)) goes on to a newer
//! value instead of reporting the loss. The slot itself needs no more: no write to it follows
//! until a lend, whose fence orders that store before the pins are loaded, and a subscriber that
//! still sees the even generation reads the value whole, since nothing writes the slot before
//! that lend.
//!
//! With `slots >= max_subscribers × MAX_PINS + 1 + depth + max_loans` the publisher always finds
//! a slot: pins hold at most `max_subscribers` of them, values kept `depth`, and loans not yet
//! published `max_loans`.
//!
//! A pool of a topic with backpressure is lossless: nothing is taken back, and only a slot that
//! every subscriber waited for has read past is lent. With none, the lend waits
//! (`backpressure.rs`); without a window a subscriber can fall at least `depth + 1` values behind
//! before a lend waits for it. Taking a subscriber out of the wait leaves its pin in place, so
//! the slot it holds stays its own.

#[cfg(loom)]
use loom::sync::atomic::fence;
#[cfg(not(loom))]
use std::sync::atomic::fence;
use std::sync::atomic::Ordering;
use std::sync::Arc;

use crate::layout::{ControlPage, LoadedPool};

/// Pin words in a subscriber's control page. A sample borrows its subscriber mutably, so a
/// subscriber holds at most one.
pub(crate) const MAX_PINS: u32 = 1;

/// The payload of a `LOAN` record: which slot holds the value, how long it is, and the slot's
/// generation when it was published. Little-endian: `u32`, `u32`, `u64`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Loan {
    pub(crate) slot: u32,
    pub(crate) len: u32,
    pub(crate) generation: u64,
}

impl Loan {
    /// Bytes of a `LOAN` record's payload.
    pub(crate) const BYTES: usize = 16;

    pub(crate) fn bytes(self) -> [u8; Self::BYTES] {
        let mut out = [0; Self::BYTES];
        out[..4].copy_from_slice(&self.slot.to_le_bytes());
        out[4..8].copy_from_slice(&self.len.to_le_bytes());
        out[8..].copy_from_slice(&self.generation.to_le_bytes());
        out
    }

    /// The loan a payload names; `None` for one of another length.
    pub(crate) fn parse(payload: &[u8]) -> Option<Self> {
        let payload: &[u8; Self::BYTES] = payload.try_into().ok()?;
        let (slot, rest) = payload.split_at(4);
        let (len, generation) = rest.split_at(4);
        Some(Self {
            slot: u32::from_le_bytes(slot.try_into().ok()?),
            len: u32::from_le_bytes(len.try_into().ok()?),
            generation: u64::from_le_bytes(generation.try_into().ok()?),
        })
    }
}

/// The generations as the publisher reaches them.
pub(crate) trait Generations {
    /// `generation[slot] = value` with `order`.
    fn store_generation(&self, slot: u32, value: u64, order: Ordering);
}

/// The subscribers' pin words as the publisher reaches them, read-only.
pub(crate) trait Pins {
    /// Into `pinned`, cleared first, the slot each pin word names: a `Relaxed` load of each, after
    /// the caller's fence. A word of 0 names none.
    fn load_pins(&self, pinned: &mut Vec<u32>);
}

/// The generations as a subscriber reaches them, read-only.
pub(crate) trait GenerationLoads {
    /// Slots in the pool.
    fn slots(&self) -> u32;
    /// `generation[slot]`, a `Relaxed` load.
    fn load_generation(&self, slot: u32) -> u64;
}

/// A subscriber's own pin word.
pub(crate) trait Pin {
    fn store_pin(&self, value: u32, order: Ordering);
}

/// Why a loan was refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LendError {
    /// As many loans as the policy allows are held and not published.
    TooMany,
    /// Every slot not lent is pinned, which is more pins than the pool was made for, or, in a
    /// lossless pool, pinned or not read past yet, which a wait for room waits out.
    NoSlot,
}

/// A slot as the publisher keeps it.
#[derive(Clone, Copy, Debug, Default)]
struct Slot {
    /// The value last stored in shared memory.
    generation: u64,
    /// Where the slot's `LOAN` record ends in the ring; 0 for a slot with no value.
    end: u64,
    lent: bool,
}

/// The publisher's account of the pool: what each slot holds, and the loans out.
pub(crate) struct Slots {
    slots: Vec<Slot>,
    lent: usize,
    max_loans: usize,
    /// Published values that stay readable, at least the newest.
    depth: usize,
    /// Backpressure (plan 15 §6.5): only a slot read past is lent, and no value is taken back.
    lossless: bool,
    /// Per lend, reused: the slots tried, as flags and in order, and the slots pins name.
    tried: Vec<bool>,
    order: Vec<usize>,
    pinned: Vec<u32>,
}

impl Slots {
    /// A pool of `count` slots, all of generation 0, which keeps the newest `depth` values and
    /// lends `max_loans` slots at once; `lossless`, every value until every subscriber reads it.
    pub(crate) fn new(count: u32, depth: usize, max_loans: usize, lossless: bool) -> Self {
        let count = count as usize;
        Self {
            slots: vec![Slot::default(); count],
            lent: 0,
            max_loans,
            depth: depth.max(1),
            lossless,
            tried: vec![false; count],
            order: Vec::with_capacity(count),
            pinned: Vec::new(),
        }
    }

    /// Lend a slot for a new value. `read` is the lowest read position of the subscribers, each
    /// as its control page reports it and no lower than where it started.
    pub(crate) fn lend(
        &mut self,
        generations: &impl Generations,
        pins: &impl Pins,
        read: u64,
    ) -> Result<u32, LendError> {
        if self.lent >= self.max_loans {
            return Err(LendError::TooMany);
        }
        self.tried.fill(false);
        self.order.clear();
        while let Some(at) = self.candidate(read) {
            self.tried[at] = true;
            self.order.push(at);
            let slot = &mut self.slots[at];
            if slot.generation % 2 == 0 {
                slot.generation += 1;
                generations.store_generation(at as u32, slot.generation, Ordering::Relaxed);
            }
            // Odd by now, from this lend, an earlier one or a retire, before the pins are loaded.
            fence(Ordering::SeqCst);
            pins.load_pins(&mut self.pinned);
            let pinned = &self.pinned;
            let free = self
                .order
                .iter()
                .find(|&&at| !pinned.contains(&(at as u32)));
            let Some(&free) = free else {
                continue;
            };
            // After a released pin: the subscriber's reads of the slot come before the next fill.
            fence(Ordering::Acquire);
            self.slots[free].lent = true;
            self.lent += 1;
            return Ok(free as u32);
        }
        Err(LendError::NoSlot)
    }

    /// Next to try: the second newest read by all (newest if alone), else oldest; lossless: none.
    fn candidate(&self, read: u64) -> Option<usize> {
        let mut newest_read: Option<(usize, u64)> = None;
        let mut next_read: Option<(usize, u64)> = None;
        let mut oldest: Option<(usize, u64)> = None;
        let open = self
            .slots
            .iter()
            .enumerate()
            .filter(|&(at, slot)| !slot.lent && !self.tried[at]);
        for (at, slot) in open {
            if slot.end <= read {
                if newest_read.is_none_or(|(_, end)| slot.end > end) {
                    next_read = newest_read;
                    newest_read = Some((at, slot.end));
                } else if next_read.is_none_or(|(_, end)| slot.end > end) {
                    next_read = Some((at, slot.end));
                }
            } else if !self.lossless && oldest.is_none_or(|(_, end)| slot.end < end) {
                oldest = Some((at, slot.end));
            }
        }
        // Its subscribers' cores still cache the value read last: a write must take it from them.
        let read = match newest_read {
            Some((_, end)) if end != 0 => next_read.or(newest_read),
            _ => newest_read,
        };
        read.or(oldest).map(|(at, _)| at)
    }

    /// Lent `slot` is written: even, `Release`; [`retire`](Self::retire) follows the commit.
    pub(crate) fn publish(&mut self, generations: &impl Generations, slot: u32, end: u64) -> u64 {
        let entry = &mut self.slots[slot as usize];
        debug_assert!(entry.lent && entry.generation % 2 == 1);
        entry.generation += 1;
        generations.store_generation(slot, entry.generation, Ordering::Release);
        entry.end = end;
        entry.lent = false;
        self.lent -= 1;
        entry.generation
    }

    /// After the newest `LOAN` record's commit, take back the values older than the newest `depth`.
    pub(crate) fn retire(&mut self, generations: &impl Generations) {
        if self.lossless {
            return;
        }
        loop {
            let mut held = 0;
            let mut oldest: Option<(usize, u64)> = None;
            let published = self
                .slots
                .iter()
                .enumerate()
                .filter(|&(_, slot)| !slot.lent && slot.end != 0 && slot.generation % 2 == 0);
            for (at, slot) in published {
                held += 1;
                if oldest.is_none_or(|(_, end)| slot.end < end) {
                    oldest = Some((at, slot.end));
                }
            }
            let Some((at, _)) = oldest.filter(|_| held > self.depth) else {
                return;
            };
            let slot = &mut self.slots[at];
            slot.generation += 1;
            generations.store_generation(at as u32, slot.generation, Ordering::Release);
        }
    }

    /// Lent `slot` comes back unpublished. Its generation stays odd: it holds no value.
    pub(crate) fn give_back(&mut self, slot: u32) {
        let entry = &mut self.slots[slot as usize];
        debug_assert!(entry.lent);
        entry.end = 0;
        entry.lent = false;
        self.lent -= 1;
    }

    /// The loans held and not published.
    #[cfg(test)]
    pub(crate) fn lent(&self) -> usize {
        self.lent
    }
}

/// Why a subscriber does not read a slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Unpinned {
    /// The publisher has taken the slot back since the record was written: the value is lost.
    Lost,
    /// The record names no slot of the pool, a length past a slot, or an odd generation.
    Corrupted(&'static str),
}

/// Pin the slot `loan` names, if it still holds that loan's value. `slot_bytes` bounds the length.
pub(crate) fn pin(
    generations: &impl GenerationLoads,
    pin: &impl Pin,
    loan: Loan,
    slot_bytes: u32,
) -> Result<(), Unpinned> {
    if loan.slot >= generations.slots() {
        return Err(Unpinned::Corrupted("loan of a slot outside the pool"));
    }
    if loan.len > slot_bytes {
        return Err(Unpinned::Corrupted("loan longer than a slot"));
    }
    if loan.generation % 2 == 1 {
        return Err(Unpinned::Corrupted("loan of a slot being written"));
    }
    // Release: a publisher that loads this pin comes after the reads of the slot pinned before.
    pin.store_pin(loan.slot + 1, Ordering::Release);
    // The pin before the generation's load: the publisher's fence orders the other two.
    fence(Ordering::SeqCst);
    let generation = generations.load_generation(loan.slot);
    // After the `Release` store of the generation: the value it published is visible.
    fence(Ordering::Acquire);
    if generation == loan.generation {
        return Ok(());
    }
    unpin(pin);
    Err(Unpinned::Lost)
}

/// Let go of the slot pinned: the reads of it come before the publisher's next writes.
pub(crate) fn unpin(pin: &impl Pin) {
    pin.store_pin(0, Ordering::Release);
}

/// Take out of `subscribers` those `ended` finds gone, in order.
///
/// `ended` may judge by a store the subscriber made `Release` after its last read of the pool,
/// loaded `Relaxed`. A subscriber taken out has its pin word loaded by no later lend, so the
/// acquire fence here, after that load, is what orders those reads before the next fill.
///
/// Neither loom nor Miri fails without this fence: both let a fence of the next lend order a
/// store that thread never loaded, which the C++ memory model does not.
pub(crate) fn take_ended<T: Clone>(subscribers: &mut Vec<T>, ended: impl Fn(&T) -> bool) -> Vec<T> {
    let mut gone = Vec::new();
    subscribers.retain(|subscriber| {
        let ended = ended(subscriber);
        if ended {
            gone.push(subscriber.clone());
        }
        !ended
    });
    if !gone.is_empty() {
        fence(Ordering::Acquire);
    }
    gone
}

/// A subscriber's way to the values of a pool: the pool mapped read-only, and its pin word.
pub(crate) struct PoolReader {
    pool: LoadedPool,
    control: Arc<ControlPage>,
}

impl PoolReader {
    pub(crate) fn new(pool: LoadedPool, control: Arc<ControlPage>) -> Self {
        Self { pool, control }
    }

    pub(crate) fn pool(&self) -> &LoadedPool {
        &self.pool
    }

    /// Pin the slot `loan` names, if it still holds that loan's value.
    pub(crate) fn pin(&self, loan: Loan) -> Result<(), Unpinned> {
        pin(&self.pool, &*self.control, loan, self.pool.slot_bytes())
    }

    /// Let go of the slot pinned.
    pub(crate) fn unpin(&self) {
        unpin(&*self.control);
    }

    /// Copy the value `loan` names into `into`, which then holds it and nothing more: pin the
    /// slot, copy it with atomic loads, let go. A publisher that broke the protocol and wrote the
    /// slot meanwhile makes the copy wrong, never undefined.
    pub(crate) fn copy(&self, loan: Loan, into: &mut Vec<u8>) -> Result<(), Unpinned> {
        self.pin(loan)?;
        let len = loan.len as usize;
        if into.capacity() < len {
            // A taken buffer, as the ring's reader makes it again: a new one, not a `reserve`.
            *into = Vec::with_capacity(len);
        } else {
            into.clear();
        }
        self.pool.load_slot_extend(loan.slot, len, into);
        self.unpin();
        Ok(())
    }

    /// [`copy`](Self::copy) for a subscriber that vouches for its publisher: a plain memory copy
    /// of the slot, into `into` emptied and not zeroed.
    ///
    /// # Safety
    ///
    /// The publisher keeps the pin protocol, and this process reads this pool through no run
    /// ([`LoadedPool::slot_slice`]'s contract): the subscriber is
    /// [`Trusted`](crate::Trusted).
    pub(crate) unsafe fn copy_vouched(
        &self,
        loan: Loan,
        into: &mut Vec<u8>,
    ) -> Result<(), Unpinned> {
        self.pin(loan)?;
        into.clear();
        // SAFETY: the pin this call holds, and the caller's word for the publisher.
        into.extend_from_slice(unsafe { self.pool.slot_slice(loan.slot, loan.len as usize) });
        self.unpin();
        Ok(())
    }
}

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;

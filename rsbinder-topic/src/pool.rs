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
//!   pin = i + 1; fence(SeqCst); generation[i] == g ? read the slot : pin = 0 and lose it
//!   done reading: pin = 0 (Release)
//! publisher, to lend slot i:
//!   generation[i] = odd; fence(SeqCst); every pin word: i + 1 ? try another slot : lend i
//!   lending it: fence(Acquire), then the slot is written
//!   published: generation[i] = even (Release), then the LOAN record
//! ```
//!
//! The pin word is the subscriber's memory and the generations the publisher's, each read-only
//! to the other side, where an atomic load is `Relaxed` alone: the `SeqCst` fences order each
//! side's store before its load. Of a subscriber that pins as the publisher takes the slot back,
//! one of the two sees the other's store: the subscriber finds the odd generation, or the
//! publisher finds the pin. A pin seen as 0 was released with `Release`, and the publisher's
//! acquire fence after it orders the subscriber's reads before the writes the publisher then
//! makes.
//!
//! # Which slot is lent
//!
//! A slot that every subscriber has read past (the `read` its control page reports is at or above
//! the end of the slot's `LOAN` record) is needed by none of them; of those, the one published
//! last is lent first, so that a pool larger than the cache keeps reusing the slots still in it
//! (plan 15 §8.4, "P3-0"). With none, the oldest is lent, which keeps the newest values longest.
//! A `read` is the subscriber's to write: a wrong one only changes which slot is lent, and the
//! generation check still decides what a subscriber reads, so it costs that subscriber values.
//!
//! With `slots >= max_subscribers × MAX_PINS + 1 + depth + max_loans` the publisher always finds
//! a slot: pins hold at most `max_subscribers` of them, and loans not yet published `max_loans`.

#![allow(
    dead_code,
    reason = "plan 15 P3-1: the loan and the sample that use these come in P3-2 and P3-3"
)]

#[cfg(loom)]
use loom::sync::atomic::fence;
#[cfg(not(loom))]
use std::sync::atomic::fence;
use std::sync::atomic::Ordering;

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
    /// Whether some pin word names `slot`: a `Relaxed` load of each, after the caller's fence.
    fn pinned(&self, slot: u32) -> bool;
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
    /// Every slot not lent is pinned: more pins than the pool was made for.
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
    /// The slots one `lend` has tried, kept to spare an allocation per loan.
    tried: Vec<bool>,
}

impl Slots {
    /// A pool of `count` slots, all of generation 0, of which `max_loans` may be lent at once.
    pub(crate) fn new(count: u32, max_loans: usize) -> Self {
        let count = count as usize;
        Self {
            slots: vec![Slot::default(); count],
            lent: 0,
            max_loans,
            tried: vec![false; count],
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
        while let Some(at) = self.candidate(read) {
            self.tried[at] = true;
            let slot = &mut self.slots[at];
            let number = at as u32;
            if slot.generation % 2 == 0 {
                slot.generation += 1;
                generations.store_generation(number, slot.generation, Ordering::Relaxed);
            }
            // The odd generation, an earlier lend's or this one's, before the pins are loaded.
            fence(Ordering::SeqCst);
            if pins.pinned(number) {
                continue;
            }
            // After a released pin: the subscriber's reads of the slot come before the next fill.
            fence(Ordering::Acquire);
            slot.lent = true;
            self.lent += 1;
            return Ok(number);
        }
        Err(LendError::NoSlot)
    }

    /// The slot to try next: of those read by every subscriber the newest, else the oldest.
    fn candidate(&self, read: u64) -> Option<usize> {
        let mut newest_read: Option<(usize, u64)> = None;
        let mut oldest: Option<(usize, u64)> = None;
        let open = self
            .slots
            .iter()
            .enumerate()
            .filter(|&(at, slot)| !slot.lent && !self.tried[at]);
        for (at, slot) in open {
            if slot.end <= read {
                if newest_read.is_none_or(|(_, end)| slot.end > end) {
                    newest_read = Some((at, slot.end));
                }
            } else if oldest.is_none_or(|(_, end)| slot.end < end) {
                oldest = Some((at, slot.end));
            }
        }
        newest_read.or(oldest).map(|(at, _)| at)
    }

    /// The value in lent `slot` is written: its even generation, stored `Release`. The commit of
    /// the `LOAN` record, after it, orders the value for a subscriber as well; `end` is where that
    /// record will end.
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

    /// Lent `slot` comes back unpublished. Its generation stays odd: it holds no value.
    pub(crate) fn give_back(&mut self, slot: u32) {
        let entry = &mut self.slots[slot as usize];
        debug_assert!(entry.lent);
        entry.end = 0;
        entry.lent = false;
        self.lent -= 1;
    }

    /// The loans held and not published.
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
    pin.store_pin(loan.slot + 1, Ordering::Relaxed);
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

#[cfg(all(test, not(loom)))]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;

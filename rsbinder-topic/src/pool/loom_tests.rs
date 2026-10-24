// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The pin protocol of `pool.rs` under loom (plan 15 AC-15.3.5): [`Slots::lend`],
//! [`Slots::publish`], [`pin`] and [`unpin`] as they are, over a model of the generations, one
//! subscriber's pin word, and the slots' bytes in loom's `UnsafeCell`. loom fails a model in which
//! a write of a slot does not happen after every read of it, or a read does not happen after the
//! write it sees, so a pinned slot written over and a value read before it was published both
//! fail. The ring's record is one atomic word, stored `Release` and loaded `Acquire` as a commit
//! and the reader's counter load are. The flawed models show the check is not vacuous: a
//! publisher that does not look at the pins, a pin released `Relaxed`. A generation published
//! `Relaxed` is no flaw here: the record's commit comes after it and orders the value as well.
//! With everything read the publisher lends the slot published second last, so with two slots it
//! lends them in turn, and the slot of a record the subscriber holds is lent again for the second
//! publish after it. With nothing read it lends the oldest, and with a depth of 1
//! that is a slot the publish before took back, which only the retire's odd generation keeps
//! from a subscriber holding its record. Run: see `Cargo.toml` (`--cfg loom`).

// The std `Arc`: one of loom's dropped while a failed model unwinds calls into loom and aborts.
use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use loom::thread;

use super::*;

const SLOTS: u32 = 2;

/// What a model leaves out of the protocol.
#[derive(Clone, Copy, PartialEq)]
enum Flaw {
    None,
    /// The publisher lends a slot whatever the pin words say.
    IgnoresPins,
    /// The subscriber stores its pin word `Relaxed`, 0 and a slot's number alike.
    RelaxedUnpin,
}

struct Model {
    generations: [AtomicU64; SLOTS as usize],
    pin: AtomicU32,
    slots: [UnsafeCell<u64>; SLOTS as usize],
    /// The newest `LOAN` record: slot, the round that filled it, generation; 0 before the first.
    record: AtomicU64,
    flaw: Flaw,
}

// SAFETY: the slots are what loom checks: every access is through `with` or `with_mut`.
unsafe impl Sync for Model {}

impl Model {
    fn new(flaw: Flaw) -> Self {
        Self {
            generations: [(); SLOTS as usize].map(|()| AtomicU64::new(0)),
            pin: AtomicU32::new(0),
            slots: [(); SLOTS as usize].map(|()| UnsafeCell::new(0)),
            record: AtomicU64::new(0),
            flaw,
        }
    }
}

impl Generations for Model {
    fn store_generation(&self, slot: u32, value: u64, order: Ordering) {
        self.generations[slot as usize].store(value, order);
    }
}

impl Pins for Model {
    fn load_pins(&self, pinned: &mut Vec<u32>) {
        pinned.clear();
        let word = self.pin.load(Ordering::Relaxed);
        if self.flaw != Flaw::IgnoresPins {
            pinned.extend(word.checked_sub(1));
        }
    }
}

impl GenerationLoads for Model {
    fn slots(&self) -> u32 {
        SLOTS
    }

    fn load_generation(&self, slot: u32) -> u64 {
        self.generations[slot as usize].load(Ordering::Relaxed)
    }
}

impl Pin for Model {
    fn store_pin(&self, value: u32, order: Ordering) {
        let order = match order {
            Ordering::Release if self.flaw == Flaw::RelaxedUnpin => Ordering::Relaxed,
            order => order,
        };
        self.pin.store(value, order);
    }
}

/// `rounds` lends as if all read to `read`; twice, the subscriber reads the newest slot if pinned.
fn a_pinned_slot_is_read_whole_and_never_written(flaw: Flaw, rounds: u64, read: u64) {
    let mut model = loom::model::Builder::new();
    model.preemption_bound = Some(3);
    model.check(move || {
        let model = Arc::new(Model::new(flaw));
        let publisher = {
            let model = model.clone();
            thread::spawn(move || {
                let mut slots = Slots::new(SLOTS, 1, 1, false);
                for round in 1..=rounds {
                    let slot = slots.lend(&*model, &*model, read).expect("a slot");
                    // SAFETY: a lent slot, which no pin holds; loom checks that it is so.
                    model.slots[slot as usize].with_mut(|value| unsafe { *value = round });
                    let generation = slots.publish(&*model, slot, round);
                    let record = u64::from(slot) | round << 8 | generation << 16;
                    model.record.store(record, Ordering::Release);
                }
            })
        };
        for _ in 0..2 {
            let record = model.record.load(Ordering::Acquire);
            if record == 0 {
                continue;
            }
            let loan = Loan {
                slot: (record & 0xff) as u32,
                len: 8,
                generation: record >> 16,
            };
            match pin(&*model, &*model, loan, 8) {
                Ok(()) => {
                    // SAFETY: a pinned slot of the generation the record names; loom checks it.
                    let value = model.slots[loan.slot as usize].with(|value| unsafe { *value });
                    assert_eq!(value, (record >> 8) & 0xff, "a pinned slot that changed");
                    unpin(&*model);
                }
                Err(Unpinned::Lost) => {}
                Err(Unpinned::Corrupted(what)) => panic!("a publisher that conforms: {what}"),
            }
        }
        publisher.join().unwrap();
    });
}

/// The subscriber reads slot 0, then pins slot 1 as the publisher tries slot 0 first.
#[test]
fn loom_pool_a_pin_moved_to_another_slot_orders_the_read_of_the_first() {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(3);
    builder.check(|| {
        let model = Arc::new(Model::new(Flaw::None));
        let mut slots = Slots::new(SLOTS, 2, 1, false);
        let mut loans = [0, 0].map(|read| {
            let slot = slots.lend(&*model, &*model, read).expect("a slot");
            Loan {
                slot,
                len: 8,
                generation: slots.publish(&*model, slot, u64::from(slot + 1)),
            }
        });
        loans.sort_by_key(|loan| loan.slot);
        let publisher = {
            let model = model.clone();
            thread::spawn(move || {
                let slot = slots.lend(&*model, &*model, u64::MAX).expect("a slot");
                // SAFETY: a lent slot, which no pin holds; loom checks that it is so.
                model.slots[slot as usize].with_mut(|value| unsafe { *value = 3 });
            })
        };
        for loan in loans {
            if pin(&*model, &*model, loan, 8).is_ok() {
                // SAFETY: a pinned slot of the generation the loan names; loom checks it.
                model.slots[loan.slot as usize].with(|value| unsafe { *value });
                unpin(&*model);
            }
        }
        publisher.join().unwrap();
    });
}

#[test]
fn loom_pool_a_pinned_slot_is_read_whole_and_never_written() {
    a_pinned_slot_is_read_whole_and_never_written(Flaw::None, 3, u64::MAX);
}

/// Nothing read: round 3 lends the slot round 2 took back, so round 1's record is lost, not read.
#[test]
fn loom_pool_a_slot_taken_back_and_lent_again_is_never_read() {
    a_pinned_slot_is_read_whole_and_never_written(Flaw::None, 3, 0);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn loom_pool_a_publisher_that_ignores_the_pins_writes_a_slot_being_read() {
    a_pinned_slot_is_read_whole_and_never_written(Flaw::IgnoresPins, 3, u64::MAX);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn loom_pool_a_pin_released_relaxed_lets_the_next_fill_race_the_read() {
    a_pinned_slot_is_read_whole_and_never_written(Flaw::RelaxedUnpin, 3, u64::MAX);
}

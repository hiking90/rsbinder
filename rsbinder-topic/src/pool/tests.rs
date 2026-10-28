// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};
use std::collections::VecDeque;

use super::*;

/// Generations and pin words in plain cells, with the stores they saw.
#[derive(Default)]
struct Fake {
    generations: RefCell<Vec<u64>>,
    stores: RefCell<Vec<(u32, u64, Ordering)>>,
    pinned: RefCell<Vec<u32>>,
    /// What the next loads of the pin words find, before `pinned` again.
    moving: RefCell<VecDeque<Vec<u32>>>,
    pin: Cell<u32>,
    pin_stores: RefCell<Vec<(u32, Ordering)>>,
}

impl Fake {
    fn new(slots: usize) -> Self {
        Self {
            generations: RefCell::new(vec![0; slots]),
            ..Self::default()
        }
    }
}

impl Generations for Fake {
    fn store_generation(&self, slot: u32, value: u64, order: Ordering) {
        self.generations.borrow_mut()[slot as usize] = value;
        self.stores.borrow_mut().push((slot, value, order));
    }
}

impl Pins for Fake {
    fn load_pins(&self, pinned: &mut Vec<u32>) {
        match self.moving.borrow_mut().pop_front() {
            Some(found) => *pinned = found,
            None => pinned.clone_from(&self.pinned.borrow()),
        }
    }
}

impl GenerationLoads for Fake {
    fn slots(&self) -> u32 {
        self.generations.borrow().len() as u32
    }

    fn load_generation(&self, slot: u32) -> u64 {
        self.generations.borrow()[slot as usize]
    }
}

impl Pin for Fake {
    fn store_pin(&self, value: u32, order: Ordering) {
        self.pin.set(value);
        self.pin_stores.borrow_mut().push((value, order));
    }
}

/// Lend and publish one value with its record ending at `end`, then retire; the slot.
fn put(slots: &mut Slots, memory: &Fake, read: u64, end: u64) -> u32 {
    let slot = slots.lend(memory, memory, read).expect("a slot");
    slots.publish(memory, slot, end);
    slots.retire(memory);
    slot
}

#[test]
fn a_loan_record_reads_back_and_another_length_is_none() {
    let loan = Loan {
        slot: 7,
        len: 0x0102_0304,
        generation: 0x1122_3344_5566_7788,
    };
    let bytes = loan.bytes();
    assert_eq!(bytes[..4], 7u32.to_le_bytes());
    assert_eq!(Loan::parse(&bytes), Some(loan));
    assert_eq!(Loan::parse(&bytes[..15]), None);
    assert_eq!(Loan::parse(&[0; 17]), None);
}

#[test]
fn a_lend_makes_the_generation_odd_and_a_publish_even_with_release() {
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 3, 1, false);
    let slot = slots.lend(&memory, &memory, 0).expect("a slot");
    assert_eq!(memory.load_generation(slot), 1);
    assert_eq!(slots.publish(&memory, slot, 24), 2);
    assert_eq!(
        *memory.stores.borrow(),
        [(slot, 1, Ordering::Relaxed), (slot, 2, Ordering::Release)]
    );
}

#[test]
fn the_slot_read_by_everyone_and_published_second_last_is_lent_first() {
    let memory = Fake::new(4);
    let mut slots = Slots::new(4, 4, 1, false);
    // Nobody has read anything: the free slots, then the oldest values.
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    let c = put(&mut slots, &memory, 0, 72);
    let d = put(&mut slots, &memory, 0, 96);
    assert_eq!([a, b, c, d], [0, 1, 2, 3]);
    assert_eq!(
        put(&mut slots, &memory, 0, 120),
        a,
        "the oldest, when none is read"
    );
    // Read up to 72: of `b` (48) and `c` (72), the older, as `c` was read last.
    assert_eq!(put(&mut slots, &memory, 72, 144), b);
    // Read past everything: the one published second last, so two slots in turn.
    assert_eq!(put(&mut slots, &memory, u64::MAX, 168), a);
    assert_eq!(put(&mut slots, &memory, u64::MAX, 192), b);
    assert_eq!(put(&mut slots, &memory, u64::MAX, 216), a);
    // The one published last is lent when no other is read past.
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 2, 1, false);
    let a = put(&mut slots, &memory, 0, 24);
    put(&mut slots, &memory, 0, 48);
    assert_eq!(put(&mut slots, &memory, 24, 72), a);
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 3, 1, false);
    for end in [24, 48, 72] {
        put(&mut slots, &memory, 0, end);
    }
    assert_eq!(put(&mut slots, &memory, 0, 96), 0, "the oldest");
    // Read past [96, 48, 72]: the second newest is slot 2, not the first other one met.
    assert_eq!(put(&mut slots, &memory, u64::MAX, 120), 2);
}

#[test]
fn a_pinned_slot_is_passed_over_and_left_odd() {
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 3, 1, false);
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    memory.pinned.borrow_mut().push(a);
    // `b` read last: `a` is tried first, pinned, taken back; the free slot is next.
    let lent = slots.lend(&memory, &memory, u64::MAX).expect("a slot");
    assert_eq!(lent, 2);
    assert_eq!(memory.load_generation(a), 3, "taken back: odd");
    assert_eq!(memory.load_generation(b), 2, "read last, not tried");
    slots.publish(&memory, lent, 72);
    // Unpinned, it is lent again without another store before its fill.
    memory.pinned.borrow_mut().clear();
    let stores = memory.stores.borrow().len();
    assert_eq!(slots.lend(&memory, &memory, 48).expect("a slot"), a);
    assert_eq!(memory.stores.borrow().len(), stores);
    assert_eq!(slots.publish(&memory, a, 96), 4);
}

#[test]
fn values_past_the_depth_are_taken_back_oldest_first() {
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 2, 1, false);
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    assert_eq!([a, b].map(|slot| memory.load_generation(slot)), [2, 2]);
    let stores = memory.stores.borrow().len();
    let c = slots.lend(&memory, &memory, 0).expect("a slot");
    slots.publish(&memory, c, 72);
    // The record of `c` is committed between these: until then `a` stays readable.
    assert_eq!(
        memory.load_generation(a),
        2,
        "a publish alone takes nothing back"
    );
    slots.retire(&memory);
    assert_eq!(
        memory.stores.borrow()[stores..],
        [
            (c, 1, Ordering::Relaxed),
            (c, 2, Ordering::Release),
            (a, 3, Ordering::Release)
        ],
        "the third value takes the first back"
    );
    let loan = |slot| Loan {
        slot,
        len: 8,
        generation: 2,
    };
    assert_eq!(pin(&memory, &memory, loan(a), 64), Err(Unpinned::Lost));
    assert_eq!(pin(&memory, &memory, loan(b), 64), Ok(()));
    unpin(&memory);
    // Taken back and the oldest: lent next, with no store before its fill.
    let stores = memory.stores.borrow().len();
    assert_eq!(slots.lend(&memory, &memory, 0).expect("a slot"), a);
    assert_eq!(memory.stores.borrow().len(), stores);

    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 0, 1, false);
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    let generations = [a, b].map(|slot| memory.load_generation(slot));
    assert_eq!(generations, [3, 2], "a depth of 0 keeps the newest");
}

#[test]
fn a_lossless_pool_lends_only_a_slot_read_past_and_takes_nothing_back() {
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 1, 1, true);
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    let c = put(&mut slots, &memory, 0, 72);
    assert_eq!([a, b, c].map(|slot| memory.load_generation(slot)), [2; 3]);
    // Nothing read: no slot, and none tried, so every value stays readable.
    let stores = memory.stores.borrow().len();
    assert_eq!(slots.lend(&memory, &memory, 0), Err(LendError::NoSlot));
    assert_eq!(memory.stores.borrow().len(), stores);
    assert_eq!(slots.lent(), 0);
    // Read up to 48: the older of `a` and `b`, as `b` was read last, and `c` unread is left alone.
    assert_eq!(slots.lend(&memory, &memory, 48), Ok(a));
    slots.publish(&memory, a, 96);
    slots.retire(&memory);
    assert_eq!(memory.load_generation(c), 2);
    memory.pinned.borrow_mut().push(b);
    assert_eq!(slots.lend(&memory, &memory, 48), Err(LendError::NoSlot));
    assert_eq!(memory.load_generation(b), 3, "sole one read, tried");
}

#[test]
fn loans_past_the_policys_bound_are_refused_and_a_given_back_slot_is_free() {
    let memory = Fake::new(4);
    let mut slots = Slots::new(4, 4, 2, false);
    let a = slots.lend(&memory, &memory, 0).expect("a slot");
    let b = slots.lend(&memory, &memory, 0).expect("a slot");
    assert_ne!(a, b);
    assert_eq!(slots.lend(&memory, &memory, 0), Err(LendError::TooMany));
    slots.give_back(a);
    assert_eq!(slots.lent(), 1);
    assert_eq!(memory.load_generation(a), 1, "no value: odd");
    // Free again, and with no value it is as good as a slot nobody needs.
    assert_eq!(slots.lend(&memory, &memory, 0).expect("a slot"), a);
}

#[test]
fn a_pin_that_moves_during_a_lend_holds_one_slot_only() {
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 2, 1, false);
    let a = put(&mut slots, &memory, u64::MAX, 24);
    // `a` read last: the free slot is tried first; the one pin word moves from it to `a`.
    memory.moving.borrow_mut().extend([vec![1 - a], vec![a]]);
    assert_eq!(slots.lend(&memory, &memory, u64::MAX), Ok(1 - a));
    assert_eq!(memory.load_generation(a), 3, "tried: odd");
}

#[test]
fn every_open_slot_pinned_is_no_slot() {
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 2, 1, false);
    memory.pinned.borrow_mut().extend([0, 1]);
    assert_eq!(slots.lend(&memory, &memory, 0), Err(LendError::NoSlot));
    assert_eq!(slots.lent(), 0);
}

#[test]
fn a_pin_holds_while_the_generation_matches_and_lets_go_when_it_does_not() {
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 2, 1, false);
    let slot = put(&mut slots, &memory, 0, 24);
    // A newer value, so that `slot` is the one a lend tries first.
    put(&mut slots, &memory, 0, 48);
    let loan = Loan {
        slot,
        len: 64,
        generation: 2,
    };
    assert_eq!(pin(&memory, &memory, loan, 64), Ok(()));
    assert_eq!(memory.pin.get(), slot + 1);
    unpin(&memory);
    assert_eq!(memory.pin.get(), 0);
    // Taken back since the record: lost, and the pin is released again.
    slots.lend(&memory, &memory, u64::MAX).expect("a slot");
    assert_eq!(pin(&memory, &memory, loan, 64), Err(Unpinned::Lost));
    assert_eq!(memory.pin.get(), 0);
    assert_eq!(
        *memory.pin_stores.borrow(),
        [
            (slot + 1, Ordering::Release),
            (0, Ordering::Release),
            (slot + 1, Ordering::Release),
            (0, Ordering::Release)
        ]
    );
}

#[test]
fn a_loan_outside_the_pool_is_corrupted_before_any_pin() {
    let memory = Fake::new(2);
    let loan = |slot, len, generation| Loan {
        slot,
        len,
        generation,
    };
    for (bad, what) in [
        (loan(2, 8, 2), "loan of a slot outside the pool"),
        (loan(0, 65, 2), "loan longer than a slot"),
        (loan(0, 8, 3), "loan of a slot being written"),
    ] {
        assert_eq!(
            pin(&memory, &memory, bad, 64),
            Err(Unpinned::Corrupted(what))
        );
    }
    assert!(memory.pin_stores.borrow().is_empty());
}

#[test]
fn only_the_subscribers_that_ended_are_taken_out_in_order() {
    let mut subscribers = vec![1, 2, 3, 4];
    assert_eq!(take_ended(&mut subscribers, |&s| s % 2 == 0), [2, 4]);
    assert_eq!(subscribers, [1, 3]);
    assert!(take_ended(&mut subscribers, |_| false).is_empty());
}

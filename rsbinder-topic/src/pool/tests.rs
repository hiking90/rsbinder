// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::cell::{Cell, RefCell};

use super::*;

/// Generations and pin words in plain cells, with the stores they saw.
#[derive(Default)]
struct Fake {
    generations: RefCell<Vec<u64>>,
    stores: RefCell<Vec<(u32, u64, Ordering)>>,
    pinned: RefCell<Vec<u32>>,
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
    fn pinned(&self, slot: u32) -> bool {
        self.pinned.borrow().contains(&slot)
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

/// Lend and publish one value with its record ending at `end`; the slot.
fn put(slots: &mut Slots, memory: &Fake, read: u64, end: u64) -> u32 {
    let slot = slots.lend(memory, memory, read).expect("a slot");
    slots.publish(memory, slot, end);
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
    let mut slots = Slots::new(3, 1);
    let slot = slots.lend(&memory, &memory, 0).expect("a slot");
    assert_eq!(memory.load_generation(slot), 1);
    assert_eq!(slots.publish(&memory, slot, 24), 2);
    assert_eq!(
        *memory.stores.borrow(),
        [(slot, 1, Ordering::Relaxed), (slot, 2, Ordering::Release)]
    );
}

#[test]
fn the_slot_read_by_everyone_and_published_last_is_lent_first() {
    let memory = Fake::new(4);
    let mut slots = Slots::new(4, 1);
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
    // Read up to 72: of `b` (48) and `c` (72), the newer.
    assert_eq!(put(&mut slots, &memory, 72, 144), c);
    // Read past everything: the one published last, again and again.
    assert_eq!(put(&mut slots, &memory, u64::MAX, 168), c);
    assert_eq!(put(&mut slots, &memory, u64::MAX, 192), c);
}

#[test]
fn a_pinned_slot_is_passed_over_and_left_odd() {
    let memory = Fake::new(3);
    let mut slots = Slots::new(3, 1);
    let a = put(&mut slots, &memory, 0, 24);
    let b = put(&mut slots, &memory, 0, 48);
    memory.pinned.borrow_mut().push(b);
    // `b` is the newest read slot; it is pinned, so it is taken back and the next one tried.
    let lent = slots.lend(&memory, &memory, u64::MAX).expect("a slot");
    assert_eq!(lent, a);
    assert_eq!(memory.load_generation(b), 3, "taken back: odd");
    slots.publish(&memory, lent, 72);
    // Unpinned, it is lent again without another store before its fill.
    memory.pinned.borrow_mut().clear();
    let stores = memory.stores.borrow().len();
    assert_eq!(slots.lend(&memory, &memory, 48).expect("a slot"), b);
    assert_eq!(memory.stores.borrow().len(), stores);
    assert_eq!(slots.publish(&memory, b, 96), 4);
}

#[test]
fn loans_past_the_policys_bound_are_refused_and_a_given_back_slot_is_free() {
    let memory = Fake::new(4);
    let mut slots = Slots::new(4, 2);
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
fn every_open_slot_pinned_is_no_slot() {
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 1);
    memory.pinned.borrow_mut().extend([0, 1]);
    assert_eq!(slots.lend(&memory, &memory, 0), Err(LendError::NoSlot));
    assert_eq!(slots.lent(), 0);
}

#[test]
fn a_pin_holds_while_the_generation_matches_and_lets_go_when_it_does_not() {
    let memory = Fake::new(2);
    let mut slots = Slots::new(2, 1);
    let slot = put(&mut slots, &memory, 0, 24);
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
            (slot + 1, Ordering::Relaxed),
            (0, Ordering::Release),
            (slot + 1, Ordering::Relaxed),
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

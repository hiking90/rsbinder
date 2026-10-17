// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loans (plan 15 P3-2): a value written straight into a slot of the pool, received as the
//! ring's values are, and the slots the publisher chooses around the subscribers' pins.

use std::os::fd::AsFd;
use std::sync::atomic::Ordering;

use rsbinder::{ExceptionCode, ParcelFileDescriptor, Status};

use crate::aidl::ITopic::WAKE_FUTEX;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::connect::{Asking, Token};
use crate::layout::{self, ControlPage, LoadedPool, MIN_RING, VERSION};
use crate::pool::{Pin, MAX_PINS};
use crate::{
    Bytes, Payload, PoolPolicy, PublishError, Publisher, PublisherPolicy, RecvError, Subscriber,
    SubscriberPolicy,
};

const SLOT: usize = 4096;

/// A topic of `max_subscribers` with a pool of 4 KiB slots.
fn pooled(max_subscribers: usize, depth: usize, max_loans: usize) -> PublisherPolicy {
    PublisherPolicy {
        max_subscribers,
        max_per_uid: max_subscribers,
        pool: Some(PoolPolicy {
            slot_bytes: SLOT,
            depth,
            max_loans,
            huge_pages: false,
        }),
        ..PublisherPolicy::default()
    }
}

fn subscribe(publisher: &Publisher<Bytes>) -> Subscriber<Bytes> {
    Subscriber::request(&publisher.as_binder(), &SubscriberPolicy::default()).unwrap()
}

fn pattern(seed: u8, len: usize) -> Vec<u8> {
    (0..len).map(|at| seed.wrapping_add(at as u8)).collect()
}

/// Lend a slot, write `value` into it and publish it; the slot's number.
fn lend(publisher: &Publisher<Bytes>, value: &[u8]) -> u32 {
    let mut loan = publisher.loan_slice(value.len()).unwrap();
    loan.copy_from_slice(value);
    let slot = loan.slot();
    loan.publish().unwrap();
    slot
}

#[test]
fn a_loaned_value_reaches_every_subscriber_between_the_rings_values() {
    let publisher = Publisher::<Bytes>::new(&pooled(2, 2, 1)).unwrap();
    let mut subscribers = [subscribe(&publisher), subscribe(&publisher)];
    publisher.publish(&Bytes(vec![1, 2, 3])).unwrap();
    lend(&publisher, &pattern(7, 1000));
    publisher.publish(&Bytes(vec![4])).unwrap();
    lend(&publisher, &pattern(9, SLOT));
    lend(&publisher, &[]);
    for subscriber in &mut subscribers {
        assert_eq!(subscriber.try_recv(), Ok(Bytes(vec![1, 2, 3])));
        assert_eq!(subscriber.try_recv(), Ok(Bytes(pattern(7, 1000))));
        assert_eq!(subscriber.try_recv(), Ok(Bytes(vec![4])));
        assert_eq!(subscriber.try_recv(), Ok(Bytes(pattern(9, SLOT))));
        assert_eq!(subscriber.try_recv(), Ok(Bytes(Vec::new())));
        assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    }
    let stats = publisher.stats();
    assert_eq!(
        (stats.published, stats.bytes),
        (5, 3 + 1000 + 1 + SLOT as u64)
    );
}

#[test]
fn a_loan_is_refused_without_a_pool_past_a_slot_past_the_bound_and_once_closed() {
    let plain = Publisher::<Bytes>::new(&PublisherPolicy {
        ring_bytes: MIN_RING,
        ..PublisherPolicy::default()
    })
    .unwrap();
    assert_eq!(plain.loan_slice(1).err(), Some(PublishError::NoPool));

    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let too_large = PublishError::TooLarge {
        bytes: SLOT + 1,
        max: SLOT,
    };
    assert_eq!(publisher.loan_slice(SLOT + 1).err(), Some(too_large));
    let held = publisher.loan_slice(8).unwrap();
    let too_many = PublishError::TooManyLoans { max: 1 };
    assert_eq!(publisher.loan_slice(8).err(), Some(too_many));
    drop(held);
    let held = publisher.loan_slice(8).unwrap();
    publisher.close(&Status::from(ExceptionCode::None));
    assert_eq!(held.publish(), Err(PublishError::Closed));
    assert_eq!(publisher.loan_slice(8).err(), Some(PublishError::Closed));
}

#[test]
fn a_dropped_loan_publishes_nothing_and_its_slot_is_lent_again() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    let loan = publisher.loan_slice(16).unwrap();
    let slot = loan.slot();
    drop(loan);
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
    assert_eq!(publisher.loan_slice(16).unwrap().slot(), slot);
    assert_eq!(publisher.stats().published, 0);
}

#[test]
fn the_slot_read_last_is_lent_again_and_a_subscriber_behind_loses_the_oldest() {
    // 2 pins, one to write, a depth of 1 and 1 loan: 5 slots.
    let publisher = Publisher::<Bytes>::new(&pooled(2, 1, 1)).unwrap();
    // With nobody subscribed every slot is read: the one published last comes back.
    let first = lend(&publisher, &[1; 8]);
    for _ in 0..4 {
        assert_eq!(lend(&publisher, &[2; 8]), first);
    }
    // A subscriber that has read nothing since it started: the free slots, then the oldest.
    let mut subscriber = subscribe(&publisher);
    let lent: Vec<u32> = (0..5u8).map(|n| lend(&publisher, &[n; 8])).collect();
    let mut distinct = lent.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 5, "{lent:?}");
    assert_eq!(lent[0], first, "read by everyone and published last");
    assert_eq!(lend(&publisher, &[5; 8]), lent[0], "the oldest");
    // The first value's slot holds the sixth now: that one is lost, the rest arrive.
    assert_eq!(subscriber.try_recv(), Err(RecvError::Lagged(1)));
    for n in 1..6u8 {
        assert_eq!(subscriber.try_recv(), Ok(Bytes(vec![n; 8])));
    }
    assert_eq!(subscriber.stats().lost, 1);
    // Read up to the end: the slot published last comes back again.
    assert_eq!(lend(&publisher, &[6; 8]), lent[0]);
}

/// A subscription made by hand, so that the test holds its control page and pin word.
fn pinning(publisher: &Publisher<Bytes>) -> (ControlPage, LoadedPool) {
    let (fd, page) = ControlPage::create(MAX_PINS).unwrap();
    let token = rsbinder::Interface::as_binder(&rsbinder::Binder::new(Token));
    let request = SubscribeRequest {
        version: VERSION as i32,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token.clone()),
        control: Some(ParcelFileDescriptor::from(fd)),
        pins: MAX_PINS as i32,
        wake: WAKE_FUTEX,
        wakeFd: None,
    };
    let endpoint = {
        let _asking = Asking::new(&token);
        publisher.acceptor().accept(&request).unwrap()
    };
    let pool = endpoint.pool.unwrap();
    let memory = pool.memory.as_ref().unwrap().as_fd();
    let mapped = layout::map_pool(memory, pool.slots as u32, pool.slotBytes as u32, usize::MAX);
    (page, mapped.unwrap())
}

/// AC-15.3.3, and AC-15.3.2 at the publisher's side: with every subscriber holding a pin, loans
/// still succeed, and a pinned slot is neither lent nor written.
#[test]
fn pinned_slots_are_not_lent_however_many_values_follow() {
    let publisher = Publisher::<Bytes>::new(&pooled(2, 1, 1)).unwrap();
    let (page, pool) = pinning(&publisher);
    let (other_page, _) = pinning(&publisher);
    let pinned = lend(&publisher, &pattern(42, SLOT));
    page.store_pin(pinned + 1, Ordering::Release);
    let other = lend(&publisher, &pattern(43, SLOT));
    other_page.store_pin(other + 1, Ordering::Release);
    for n in 0..5 * 4 {
        let lent = lend(&publisher, &pattern(n, SLOT));
        assert!(lent != pinned && lent != other, "value {n} in slot {lent}");
    }
    let mut held = vec![0; SLOT];
    pool.load_slot(pinned, &mut held);
    assert_eq!(held, pattern(42, SLOT));
    pool.load_slot(other, &mut held);
    assert_eq!(held, pattern(43, SLOT));
    // Let go: the subscribers have read nothing, so the oldest slots go round, this one too.
    page.store_pin(0, Ordering::Release);
    assert!((0..5).any(|n| lend(&publisher, &[n; 8]) == pinned));
}

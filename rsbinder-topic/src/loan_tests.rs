// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Loans (plan 15 §6): a value written straight into a slot of the pool, received as the
//! ring's values are, and the slots the publisher chooses around the subscribers' pins.

use std::cell::{Cell, RefCell};
use std::os::fd::AsFd;
use std::rc::Rc;
use std::sync::atomic::Ordering;

use rsbinder::ParcelFileDescriptor;

use crate::aidl::ITopic::WAKE_FUTEX;
use crate::aidl::SubscribeRequest::SubscribeRequest;
use crate::connect::{Asking, Token};
use crate::layout::{self, ControlPage, LoadedPool, MIN_RING, VERSION};
use crate::pool::{Pin, MAX_PINS};
use crate::publish::AFTER_LOAN_RECORD;
use crate::receive::LATEST_RETRIES;
use crate::ring::BEFORE_JUDGING;
use crate::{
    Bytes, Encoding, Payload, PoolPolicy, PublishError, Publisher, PublisherPolicy, Record,
    RecvError, Subscriber, SubscriberPolicy,
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
    // A depth of 3: the three loans stay readable until the subscribers come.
    let publisher = Publisher::<Bytes>::new(&pooled(2, 3, 1)).unwrap();
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
    publisher.close();
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
fn two_slots_read_by_everyone_are_lent_in_turn_and_a_subscriber_behind_gets_the_newest_depth() {
    // 2 pins, one to write, a depth of 2 and 1 loan: 6 slots.
    let publisher = Publisher::<Bytes>::new(&pooled(2, 2, 1)).unwrap();
    // With nobody subscribed every slot is read: the one published second last comes back.
    let first = lend(&publisher, &[1; 8]);
    let second = lend(&publisher, &[1; 8]);
    assert_ne!(first, second);
    for _ in 0..2 {
        assert_eq!(lend(&publisher, &[2; 8]), first);
        assert_eq!(lend(&publisher, &[2; 8]), second);
    }
    // A subscriber that has read nothing since it started: the free slots, then the oldest.
    let mut subscriber = subscribe(&publisher);
    let lent: Vec<u32> = (0..6u8).map(|n| lend(&publisher, &[n; 8])).collect();
    let mut distinct = lent.clone();
    distinct.sort_unstable();
    distinct.dedup();
    assert_eq!(distinct.len(), 6, "{lent:?}");
    assert_eq!(lent[0], first, "read by everyone and published second last");
    assert_eq!(
        lent[5], second,
        "read by everyone and published last, the only one left"
    );
    assert_eq!(lend(&publisher, &[6; 8]), lent[0], "the oldest");
    // Seven values behind, depth 2: the five oldest are taken back and the newest two arrive.
    for _ in 0..5 {
        assert_eq!(subscriber.try_recv(), Err(RecvError::Lagged(1)));
    }
    for n in 5..7u8 {
        assert_eq!(subscriber.try_recv(), Ok(Bytes(vec![n; 8])));
    }
    assert_eq!(subscriber.stats().lost, 5);
    // Read up to the end: the slot published second last comes back.
    assert_eq!(lend(&publisher, &[7; 8]), lent[5]);
}

/// Bytes under the type name of [`Bytes`], decoded where they lie: the buffer stays for the next.
#[derive(Debug)]
struct InPlace(Vec<u8>);

impl Payload for InPlace {
    fn type_name() -> &'static str {
        Bytes::type_name()
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> rsbinder::Result<()> {
        out.borrow(&self.0);
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> rsbinder::Result<Self> {
        Ok(Self(record.bytes().to_vec()))
    }
}

#[test]
fn loaned_values_copied_into_a_kept_buffer_hold_their_own_bytes_alone() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 3, 1)).unwrap();
    let policy = SubscriberPolicy::default();
    let mut subscriber = Subscriber::<InPlace>::request(&publisher.as_binder(), &policy).unwrap();
    // Shorter than the buffer the first copy leaves, then longer.
    let values = [pattern(1, 3000), pattern(2, 1000), pattern(3, 4000)];
    for value in &values {
        lend(&publisher, value);
    }
    for value in values {
        let InPlace(got) = subscriber.try_recv().unwrap();
        assert!(got == value, "{} bytes for {}", got.len(), value.len());
    }
}

/// AC-15.3.1: a lent value lies in its slot, a ring value in the subscriber; both read alike.
#[test]
fn a_lent_sample_lies_in_the_pool_and_a_ring_sample_in_the_subscriber() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    let pool = subscriber.pool().unwrap().addresses();
    lend(&publisher, &pattern(5, 3000));
    publisher.publish(&Bytes(pattern(6, 100))).unwrap();
    for (lent, value) in [(true, pattern(5, 3000)), (false, pattern(6, 100))] {
        let mut sample = subscriber.try_recv_sample().unwrap();
        assert_eq!((sample.is_loaned(), sample.len()), (lent, value.len()));
        assert_eq!(sample.to_owned(), value);
        let mut copied = vec![0; value.len()];
        sample.copy_to(&mut copied);
        assert_eq!(copied, value);
        // SAFETY: the publisher is this crate's, in this process.
        let bytes = unsafe { sample.as_slice() };
        assert_eq!(bytes, value);
        assert_eq!(pool.contains(&(bytes.as_ptr() as usize)), lent);
    }
    assert_eq!(subscriber.try_recv_sample().err(), Some(RecvError::Empty));
    assert_eq!(subscriber.stats().received, 2);
}

/// AC-15.3.6: a trusted subscriber derefs a lent value in place; its reads match untrusted copies.
#[test]
fn a_trusted_subscriber_reads_a_lent_value_in_place_and_copies_the_same_bytes() {
    // 2 pins, one to write, a depth of 4 and 1 loan: the three values stay readable.
    let publisher = Publisher::<Bytes>::new(&pooled(2, 4, 1)).unwrap();
    let mut untrusted = subscribe(&publisher);
    let mut first = subscribe(&publisher);
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(first.try_recv(), Ok(Bytes(vec![1])));
    assert_eq!(untrusted.try_recv(), Ok(Bytes(vec![1])));
    // SAFETY: the publisher is this crate's, in this process.
    let mut trusted = unsafe { first.trust_publisher() };
    let pool = trusted.pool().unwrap().addresses();
    lend(&publisher, &pattern(5, 3000));
    publisher.publish(&Bytes(pattern(6, 100))).unwrap();
    lend(&publisher, &pattern(7, SLOT));
    for (lent, value) in [(true, pattern(5, 3000)), (false, pattern(6, 100))] {
        let sample = trusted.try_recv_sample().unwrap();
        assert_eq!((sample.is_loaned(), &*sample), (lent, &value[..]));
        assert_eq!(pool.contains(&(sample.as_ptr() as usize)), lent);
        assert_eq!(sample.to_owned(), value);
        let mut copied = vec![0; value.len()];
        sample.copy_to(&mut copied);
        assert_eq!(copied, value);
        drop(sample);
        assert_eq!(untrusted.try_recv_sample().unwrap().to_owned(), value);
    }
    assert_eq!(trusted.try_recv(), Ok(Bytes(pattern(7, SLOT))));
    assert_eq!(untrusted.try_recv(), Ok(Bytes(pattern(7, SLOT))));
    assert_eq!(trusted.try_recv_sample().err(), Some(RecvError::Empty));
    assert_eq!((trusted.stats().received, trusted.stats().lost), (4, 0));
}

/// AC-15.3.2: a held sample's slot stays as it was; the subscriber loses values that went round.
#[test]
fn a_held_sample_is_not_written_however_many_values_follow() {
    // 1 pin, one to write, a depth of 1 and 1 loan: 4 slots.
    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    lend(&publisher, &pattern(42, SLOT));
    let mut sample = subscriber.try_recv_sample().unwrap();
    for n in 0..4 * 4 {
        lend(&publisher, &pattern(n, SLOT));
    }
    assert_eq!(sample.to_owned(), pattern(42, SLOT));
    // SAFETY: the publisher is this crate's, in this process.
    assert_eq!(unsafe { sample.as_slice() }, pattern(42, SLOT));
    drop(sample);
    let (mut received, mut lost, mut last) = (0, 0, None);
    loop {
        match subscriber.try_recv_sample() {
            Ok(sample) => {
                received += 1;
                last = Some(sample.to_owned());
            }
            Err(RecvError::Lagged(n)) => lost += n,
            Err(RecvError::Empty) => break,
            Err(other) => panic!("{other}"),
        }
    }
    assert_eq!(received + lost, 16, "{received} received, {lost} lost");
    assert!(lost > 0);
    assert_eq!(last, Some(pattern(15, SLOT)));
}

/// Depth 1: a value taken back as `try_latest` reads it gives way to the newer one, with no loss.
#[test]
fn the_newest_value_taken_back_as_it_is_read_gives_way_to_the_newer_one() {
    let publisher = Rc::new(Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap());
    let mut subscriber = subscribe(&publisher);
    lend(&publisher, &pattern(1, 64));
    let mut next = Some(publisher.clone());
    BEFORE_JUDGING.set(Some(Box::new(move || {
        if let Some(publisher) = next.take() {
            lend(&publisher, &pattern(2, 64));
        }
    })));
    let got = subscriber.try_latest();
    BEFORE_JUDGING.set(None);
    assert_eq!(got, Ok(Bytes(pattern(2, 64))));
    assert_eq!(subscriber.stats().lost, 0);
    assert_eq!(subscriber.try_latest(), Err(RecvError::Empty));
}

/// With a depth of 1, the value before a loan's is readable until that loan's record is.
#[test]
fn a_loan_takes_the_value_before_it_back_only_after_its_record_is_committed() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let subscriber = Rc::new(RefCell::new(subscribe(&publisher)));
    lend(&publisher, &pattern(1, 64));
    let (reading, seen) = (subscriber.clone(), Rc::new(RefCell::new(Vec::new())));
    let saw = seen.clone();
    AFTER_LOAN_RECORD.set(Some(Box::new(move || {
        saw.borrow_mut().push(reading.borrow_mut().try_recv());
    })));
    lend(&publisher, &pattern(2, 64));
    AFTER_LOAN_RECORD.set(None);
    assert_eq!(*seen.borrow(), [Ok(Bytes(pattern(1, 64)))]);
    let mut subscriber = subscriber.borrow_mut();
    assert_eq!(subscriber.try_recv(), Ok(Bytes(pattern(2, 64))));
    assert_eq!(subscriber.stats().lost, 0);
}

/// A publish at every judgment: past [`LATEST_RETRIES`] values taken back, one loss is reported.
#[test]
fn a_publisher_that_keeps_taking_the_newest_value_back_holds_no_try_latest() {
    let publisher = Rc::new(Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap());
    let mut subscriber = subscribe(&publisher);
    lend(&publisher, &pattern(0, 64));
    let published = Rc::new(Cell::new(1u8));
    let (lending, count) = (publisher.clone(), published.clone());
    BEFORE_JUDGING.set(Some(Box::new(move || {
        let n = count.get();
        if n < 64 {
            lend(&lending, &pattern(n, 64));
            count.set(n + 1);
        }
    })));
    let got = subscriber.try_latest();
    BEFORE_JUDGING.set(None);
    assert_eq!(got, Err(RecvError::Lagged(1)));
    assert_eq!(subscriber.stats().lost, 1);
    let published = published.get();
    assert_eq!(published, LATEST_RETRIES as u8 + 2, "{published} values");
    assert_eq!(
        subscriber.try_latest(),
        Ok(Bytes(pattern(published - 1, 64)))
    );
}

/// The newest value, lent, is received copied or where it lies; the values skipped are not lost.
#[test]
fn the_newest_lent_value_is_received_past_the_values_before_it_with_no_loss() {
    // A depth of 3: no value is taken back before it is skipped.
    let publisher = Publisher::<Bytes>::new(&pooled(1, 3, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    let pool = subscriber.pool().unwrap().addresses();
    lend(&publisher, &pattern(1, 64));
    publisher.publish(&Bytes(vec![2])).unwrap();
    lend(&publisher, &pattern(3, 64));
    assert_eq!(subscriber.try_latest(), Ok(Bytes(pattern(3, 64))));
    assert_eq!(subscriber.try_latest(), Err(RecvError::Empty));
    lend(&publisher, &pattern(4, 64));
    publisher.publish(&Bytes(vec![5])).unwrap();
    lend(&publisher, &pattern(6, 64));
    let mut sample = subscriber.try_latest_sample().unwrap();
    assert_eq!(
        (sample.is_loaned(), sample.to_owned()),
        (true, pattern(6, 64))
    );
    // SAFETY: the publisher is this crate's, in this process.
    let bytes = unsafe { sample.as_slice() };
    assert!(pool.contains(&(bytes.as_ptr() as usize)));
    drop(sample);
    assert_eq!(subscriber.try_latest_sample().err(), Some(RecvError::Empty));
    publisher.publish(&Bytes(vec![7])).unwrap();
    let sample = subscriber.try_latest_sample().unwrap();
    assert_eq!((sample.is_loaned(), sample.to_owned()), (false, vec![7]));
    drop(sample);
    let stats = subscriber.stats();
    assert_eq!((stats.received, stats.lost), (3, 0));
}

/// Depth 1, a lend while the newest is judged: the sample is the newer value, with no loss.
#[test]
fn the_newest_sample_taken_back_as_it_is_read_gives_way_to_the_newer_one() {
    let publisher = Rc::new(Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap());
    let mut subscriber = subscribe(&publisher);
    lend(&publisher, &pattern(1, 64));
    let mut next = Some(publisher.clone());
    BEFORE_JUDGING.set(Some(Box::new(move || {
        if let Some(publisher) = next.take() {
            lend(&publisher, &pattern(2, 64));
        }
    })));
    let got = subscriber
        .try_latest_sample()
        .map(|sample| sample.to_owned());
    BEFORE_JUDGING.set(None);
    assert_eq!(got, Ok(pattern(2, 64)));
    assert_eq!(subscriber.stats().lost, 0);
    assert_eq!(subscriber.try_latest_sample().err(), Some(RecvError::Empty));
}

#[test]
fn a_dropped_sample_lets_go_of_its_slot() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 2, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    let slot = lend(&publisher, &[3; 8]);
    lend(&publisher, &[4; 8]);
    let sample = subscriber.try_recv_sample().unwrap();
    assert_eq!(publisher.stats().subscribers[0].pinned, Some(slot));
    drop(sample);
    assert_eq!(publisher.stats().subscribers[0].pinned, None);
    // The free slots first; then the only one read past, as nothing pins it.
    for n in 0..3u8 {
        assert_ne!(lend(&publisher, &[n; 8]), slot);
    }
    assert_eq!(publisher.loan_slice(8).unwrap().slot(), slot);
}

#[test]
fn a_sample_is_waited_for_until_a_value_a_bound_or_the_end() {
    let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
    let mut subscriber = subscribe(&publisher);
    let bound = std::time::Duration::from_millis(20);
    assert_eq!(
        subscriber.recv_sample_timeout(bound).err(),
        Some(RecvError::Timeout)
    );
    std::thread::scope(|scope| {
        scope.spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(20));
            lend(&publisher, &pattern(1, 64));
        });
        let sample = subscriber.recv_sample().unwrap();
        assert_eq!(sample.to_owned(), pattern(1, 64));
    });
    publisher.close();
    assert_eq!(
        subscriber.recv_sample().err(),
        Some(RecvError::Closed(crate::Closed::End))
    );
}

/// A subscription made by hand, so that the test holds its control page and pin word.
fn pinning(publisher: &Publisher<Bytes>) -> (ControlPage, LoadedPool) {
    let (fd, page) = ControlPage::create(MAX_PINS).unwrap();
    let token = Token::detached();
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
        let _asking = Asking::new(&request);
        publisher.acceptor().accept(&request).unwrap()
    };
    let pool = endpoint.pool.unwrap();
    let memory = pool.memory.as_ref().unwrap().as_fd();
    let mapped = layout::map_pool(memory, pool.slots as u32, pool.slotBytes as u32, usize::MAX);
    (page, mapped.unwrap())
}

/// AC-15.3.3, AC-15.3.2: with every subscriber holding a pin, no pinned slot is lent or written.
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

/// Samples received by an async caller, on the blocking pool and on the reactor.
#[cfg(feature = "tokio")]
mod async_samples {
    use std::collections::BTreeSet;
    use std::future::Future;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};
    use std::time::{Duration, Instant};

    use super::*;
    use crate::{Sample, Trusted, Untrusted, Wake};

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_io()
            .enable_time()
            .build()
            .unwrap()
    }

    fn woken_by(publisher: &Publisher<Bytes>, wake: Wake) -> Subscriber<Bytes> {
        let policy = SubscriberPolicy {
            wake,
            ..SubscriberPolicy::default()
        };
        Subscriber::request(&publisher.as_binder(), &policy).unwrap()
    }

    /// Poll `future` once inside `runtime`, with a waker that does nothing.
    fn poll_once<F: Future>(
        runtime: &tokio::runtime::Runtime,
        future: std::pin::Pin<&mut F>,
    ) -> Poll<F::Output> {
        let _enter = runtime.enter();
        future.poll(&mut Context::from_waker(Waker::noop()))
    }

    #[test]
    fn an_async_sample_receive_sleeps_until_a_value_is_lent() {
        let runtime = runtime();
        for wake in [Wake::Futex, Wake::Fd] {
            let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
            let mut subscriber = woken_by(&publisher, wake);
            let local = subscriber.local().clone();
            let got = std::thread::scope(|scope| {
                scope.spawn(|| {
                    let began = Instant::now();
                    while !local.is_parked() {
                        assert!(began.elapsed() < Duration::from_secs(10), "no wait slept");
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    lend(&publisher, &pattern(1, 64));
                });
                runtime.block_on(async {
                    let waiting = subscriber.recv_sample_async();
                    let waited = tokio::time::timeout(Duration::from_secs(10), waiting).await;
                    let sample = waited.map_err(|_| RecvError::Timeout)??;
                    Ok::<_, RecvError>((sample.is_loaned(), sample.to_owned()))
                })
            });
            assert_eq!(got, Ok((true, pattern(1, 64))), "{wake:?}");
            assert_eq!(subscriber.stats().received, 1);
        }
    }

    /// A sample is returned in the poll that pins it, so a dropped `Pending` receive holds no pin.
    #[test]
    fn a_dropped_async_sample_receive_holds_no_pin() {
        let runtime = runtime();
        for wake in [Wake::Futex, Wake::Fd] {
            // 1 pin, one to write, a depth of 1 and 1 loan: 4 slots.
            let publisher = Publisher::<Bytes>::new(&pooled(1, 1, 1)).unwrap();
            let mut subscriber = woken_by(&publisher, wake);
            {
                let mut future = pin!(subscriber.recv_sample_async());
                assert!(
                    poll_once(&runtime, future.as_mut()).is_pending(),
                    "nothing lent"
                );
            }
            lend(&publisher, &pattern(1, 64));
            {
                let mut future = pin!(subscriber.recv_sample_async());
                let Poll::Ready(Ok(sample)) = poll_once(&runtime, future.as_mut()) else {
                    panic!("{wake:?}: a value lent, and no sample from the poll that looked");
                };
                assert_eq!(sample.to_owned(), pattern(1, 64));
            }
            // No slot is pinned: the values that follow are lent into every slot of the pool.
            let lent: BTreeSet<u32> = (0..16).map(|n| lend(&publisher, &[n; 8])).collect();
            assert_eq!(lent.len(), 4, "{wake:?}: {lent:?}");
        }
    }

    #[test]
    fn an_async_sample_receive_and_its_sample_move_between_threads() {
        fn moved<T: Send>(_: &T) {}
        fn movable<T: Send>() {}
        let publisher = Publisher::<Bytes>::new(&pooled(2, 1, 1)).unwrap();
        let mut subscriber = subscribe(&publisher);
        moved(&subscriber.recv_sample_async());
        // SAFETY: the publisher is this crate's, in this process.
        let mut trusted = unsafe { subscribe(&publisher).trust_publisher() };
        moved(&trusted.recv_sample_async());
        movable::<Sample<'static, Untrusted>>();
        movable::<Sample<'static, Trusted>>();
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The sizes a publisher builder works out, and the settings it refuses (plan 15 field AC-F.13).

use std::num::NonZeroUsize;
use std::time::Duration;

use rsbinder::ExceptionCode;

use crate::publish::Spec;
use crate::{Bytes, EventTopic, Publisher, PublisherBuilder, SubscriberBuilder};

fn spec(builder: PublisherBuilder<Bytes, EventTopic>) -> Spec {
    builder.event_spec().expect("a spec")
}

fn ring(builder: PublisherBuilder<Bytes, EventTopic>) -> usize {
    spec(builder).ring_bytes
}

#[test]
fn the_ring_holds_keep_records_of_max_value_and_the_one_being_written() {
    let events = Publisher::<Bytes>::events;
    assert_eq!(
        ring(events().max_value_bytes(256).keep(100)),
        101 * (256 + 8)
    );
    assert_eq!(
        ring(events().max_value_bytes(1 << 20).keep(20)),
        21 * ((1 << 20) + 8)
    );
    // Rounded up to 8 bytes, as a record is.
    assert_eq!(ring(events().max_value_bytes(4093).keep(3)), 4 * (4096 + 8));
    // `keep` defaults to 1, and 0 counts as 1.
    assert_eq!(ring(events().max_value_bytes(8192)), 2 * (8192 + 8));
    assert_eq!(ring(events().max_value_bytes(8192).keep(0)), 2 * (8192 + 8));
}

#[test]
fn a_ring_is_no_smaller_than_4_kib() {
    let events = Publisher::<Bytes>::events;
    assert_eq!(ring(events()), 4 << 10);
    assert_eq!(ring(events().keep(100)), 4 << 10);
    assert_eq!(ring(events().max_value_bytes(2040)), 4 << 10);
    // Without `max_value_bytes` a kept record is the 8 bytes of a header and a value of 8.
    assert_eq!(ring(events().keep(255)), 4 << 10);
    assert_eq!(ring(events().keep(256)), 257 * 16);
    assert_eq!(ring(events().keep(1000)), 1001 * 16);
}

#[test]
fn a_topic_with_a_pool_keeps_as_many_records_of_lent_values() {
    let events = Publisher::<Bytes>::events;
    assert_eq!(ring(events().lend(64).keep(1000)), 1001 * 24);
    assert_eq!(
        ring(events().max_value_bytes(4).lend(64).keep(1000)),
        1001 * 24
    );
    assert_eq!(
        ring(events().max_value_bytes(100).lend(64).keep(1000)),
        1001 * (104 + 8)
    );
}

#[test]
fn ring_bytes_takes_the_place_of_the_size_worked_out() {
    let events = Publisher::<Bytes>::events;
    assert_eq!(
        ring(
            events()
                .max_value_bytes(1 << 20)
                .keep(20)
                .ring_bytes(64 << 10)
        ),
        64 << 10
    );
    // The last call is the one kept: no setting is fixed by the order.
    assert_eq!(
        ring(events().ring_bytes(64 << 10).max_value_bytes(256).keep(100)),
        64 << 10
    );
    assert_eq!(
        ring(events().ring_bytes(8 << 10).ring_bytes(16 << 10)),
        16 << 10
    );
}

#[test]
fn the_settings_reach_the_spec() {
    let window = NonZeroUsize::new(2).expect("nonzero");
    let made = spec(
        Publisher::<Bytes>::events()
            .max_subscribers(4)
            .max_per_uid(2)
            .lend(4096)
            .lend_keep(3)
            .max_loans(2)
            .lend_huge_pages()
            .lossless_window(Duration::from_millis(5), window)
            .authorize(|_| true),
    );
    assert_eq!((made.max_subscribers, made.max_per_uid), (4, 2));
    let pool = made.pool.expect("a pool");
    assert_eq!((pool.slot_bytes, pool.depth, pool.max_loans), (4096, 3, 2));
    assert!(pool.huge_pages);
    let lossless = made.backpressure.expect("lossless");
    assert_eq!(lossless.timeout, Duration::from_millis(5));
    assert_eq!(lossless.window, Some(window));
    assert!(made.authorize.is_some());

    let plain = spec(Publisher::<Bytes>::events());
    assert_eq!((plain.max_subscribers, plain.max_per_uid), (64, 64));
    assert!(plain.pool.is_none() && plain.backpressure.is_none() && plain.authorize.is_none());
    // `lossless` after `lossless_window` drops the window: the last call is kept whole.
    let after = spec(
        Publisher::<Bytes>::events()
            .lossless_window(Duration::from_secs(1), window)
            .lossless(Duration::from_secs(2)),
    );
    let lossless = after.backpressure.expect("lossless");
    assert_eq!(
        (lossless.timeout, lossless.window),
        (Duration::from_secs(2), None)
    );
    // The defaults of a pool: 2 values kept, 1 loan.
    let pool = spec(Publisher::<Bytes>::events().lend(1 << 20))
        .pool
        .expect("a pool");
    assert_eq!((pool.depth, pool.max_loans, pool.huge_pages), (2, 1, false));
}

#[test]
fn a_setting_of_the_pool_without_lend_is_refused() {
    let events = Publisher::<Bytes>::events;
    for (builder, setting) in [
        (events().max_loans(2), "max_loans"),
        (events().lend_keep(3), "lend_keep"),
        (events().lend_huge_pages(), "lend_huge_pages"),
    ] {
        let refused = builder.event_spec().expect_err(setting);
        assert_eq!(refused.exception_code(), ExceptionCode::IllegalArgument);
        assert!(
            refused.to_string().contains(setting),
            "{setting}: {refused}"
        );
    }
}

#[test]
fn builders_are_shared_across_threads() {
    fn shared<T: Send + Sync + Clone>() {}
    shared::<PublisherBuilder<Bytes, EventTopic>>();
    shared::<SubscriberBuilder<Bytes>>();
}

#[cfg(all(
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod made {
    use rsbinder::StatusCode;

    use super::*;
    use crate::subscriber::Settings;
    use crate::Subscriber;

    #[test]
    fn a_topic_has_the_ring_its_builder_worked_out() {
        let events = Publisher::<Bytes>::events;
        let made = |builder: PublisherBuilder<Bytes, EventTopic>| {
            builder.build().expect("a topic").ring_bytes()
        };
        assert_eq!(made(events()), 4 << 10);
        assert_eq!(made(events().max_value_bytes(256).keep(100)), 101 * 264);
        assert_eq!(made(events().ring_bytes(64 << 10)), 64 << 10);
        // Rounded up to a multiple of 8.
        assert_eq!(made(events().ring_bytes((8 << 10) + 1)), (8 << 10) + 8);
    }

    #[test]
    fn a_ring_past_1_gib_is_refused_worked_out_or_set() {
        let events = Publisher::<Bytes>::events;
        for builder in [
            events().max_value_bytes(1 << 20).keep(1 << 10),
            events().max_value_bytes(usize::MAX).keep(usize::MAX),
            events().ring_bytes((1 << 30) + 8),
            events().ring_bytes(2 << 10),
        ] {
            let refused = builder.build().expect_err("refused");
            assert_eq!(refused.exception_code(), ExceptionCode::IllegalArgument);
        }
        let largest = events().ring_bytes(1 << 30).event_spec().expect("a spec");
        assert_eq!(largest.ring_bytes, 1 << 30);
    }

    #[test]
    fn a_default_topic_carries_values_of_up_to_2040_bytes() {
        let publisher = Publisher::<Bytes>::events().build().expect("a topic");
        assert_eq!(publisher.publish(&Bytes(vec![0; 2040])), Ok(()));
        assert_eq!(
            publisher.publish(&Bytes(vec![0; 2041])),
            Err(crate::PublishError::TooLarge {
                bytes: 2041,
                max: 2040
            })
        );
    }

    fn refused(subscribed: rsbinder::BinderResult<Subscriber<Bytes>>) -> StatusCode {
        subscribed
            .map(drop)
            .expect_err("refused")
            .transaction_error()
    }

    #[test]
    fn a_subscriber_maps_by_default_what_the_pointer_width_allows() {
        let settings = Settings::default();
        let expected = match cfg!(target_pointer_width = "64") {
            true => (256 << 20, 1 << 30),
            false => (16 << 20, 256 << 20),
        };
        assert_eq!((settings.max_ring_bytes, settings.max_pool_bytes), expected);
    }

    /// Past the limit of a 32-bit target and within that of a 64-bit one.
    #[test]
    fn a_ring_is_mapped_as_the_pointer_width_or_the_builder_allows() {
        const RING: usize = 32 << 20;
        let publisher = Publisher::<Bytes>::events()
            .ring_bytes(RING)
            .build()
            .expect("a topic");
        let topic = publisher.as_binder();
        let builder = Subscriber::<Bytes>::builder;
        let by_default = builder().subscribe(&topic);
        match cfg!(target_pointer_width = "64") {
            true => assert!(by_default.is_ok()),
            false => assert_eq!(refused(by_default), StatusCode::BadValue),
        }
        assert!(builder().max_ring_bytes(RING).subscribe(&topic).is_ok());
        let lower = builder().max_ring_bytes(RING - 8).subscribe(&topic);
        assert_eq!(refused(lower), StatusCode::BadValue);
    }

    /// The limits themselves, with the memory they name.
    #[cfg(target_pointer_width = "64")]
    #[test]
    #[ignore = "writes over 2 GiB of memory, 1 GiB of it at once"]
    fn a_default_subscriber_takes_rings_and_pools_up_to_its_limits() {
        const RING: usize = 256 << 20;
        const SLOT: usize = 256 << 20;
        let builder = Subscriber::<Bytes>::builder;
        let ring = |bytes| {
            let builder = Publisher::<Bytes>::events().ring_bytes(bytes);
            builder.build().expect("a topic")
        };
        assert!(builder().subscribe(&ring(RING).as_binder()).is_ok());
        let past = ring(RING + 8);
        let refusal = builder().subscribe(&past.as_binder());
        assert_eq!(refused(refusal), StatusCode::BadValue);
        let set = builder().max_ring_bytes(usize::MAX);
        assert!(set.subscribe(&past.as_binder()).is_ok());
        drop(past);

        // One subscriber's slot, one to lend, one kept and one loan: four slots.
        let pool = |slot_bytes| {
            let builder = Publisher::<Bytes>::events().max_subscribers(1);
            let builder = builder.lend(slot_bytes).lend_keep(1);
            builder.build().expect("a topic")
        };
        let within = pool(SLOT - 4096);
        assert!(builder().subscribe(&within.as_binder()).is_ok());
        drop(within);
        let past = pool(SLOT);
        let refusal = builder().subscribe(&past.as_binder());
        assert_eq!(refused(refusal), StatusCode::BadValue);
        let set = builder().max_pool_bytes(usize::MAX);
        assert!(set.subscribe(&past.as_binder()).is_ok());
    }
}

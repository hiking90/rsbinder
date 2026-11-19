// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `tracing` events of a subscription's life, collected on the thread that makes them:
//! within one process the subscription call and the publish run on the caller's.
//!
//! One process-wide dispatcher hands each event to the collector of the thread that makes it.
//! A thread-local dispatcher (`with_default`) is not used: while it is the only one, the
//! interest of a callsite another test's thread reaches first is cached as that thread's
//! `never`, and the collecting thread then loses the event.

use std::cell::RefCell;
use std::sync::Once;
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata};

use crate::{Bytes, Publisher, RecvError, Start, Subscriber};

thread_local! {
    /// The message and `reason` of each event of this crate's target on this thread, in order.
    static COLLECTED: RefCell<Option<Vec<(String, String)>>> = const { RefCell::new(None) };
}

#[derive(Default)]
struct Fields {
    message: String,
    reason: String,
}

impl Visit for Fields {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        match field.name() {
            "message" => self.message = format!("{value:?}"),
            "reason" => self.reason = format!("{value:?}"),
            _ => {}
        }
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        match field.name() {
            "reason" => self.reason = value.to_owned(),
            _ => self.record_debug(field, &value),
        }
    }
}

struct Forward;

impl tracing::Subscriber for Forward {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "rsbinder_topic"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        COLLECTED.with_borrow_mut(|collected| {
            if let Some(collected) = collected {
                let mut fields = Fields::default();
                event.record(&mut fields);
                collected.push((fields.message, fields.reason));
            }
        });
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

/// The events `f` makes on this thread.
fn collected(f: impl FnOnce()) -> Vec<(String, String)> {
    static FORWARD: Once = Once::new();
    FORWARD.call_once(|| {
        tracing::subscriber::set_global_default(Forward).unwrap();
        // A callsite first reached by another thread during the install may have cached `never`.
        tracing::callsite::rebuild_interest_cache();
    });
    COLLECTED.set(Some(Vec::new()));
    f();
    COLLECTED.take().unwrap()
}

#[test]
fn a_subscriptions_life_is_an_event_at_each_step_and_none_per_record() {
    let messages = collected(|| {
        let publisher = Publisher::<Bytes>::events()
            .max_subscribers(1)
            .lossless(Duration::from_millis(10))
            .build()
            .unwrap();
        let topic = publisher.as_binder();
        let builder = || Subscriber::<Bytes>::builder().start_at(Start::Next);
        let mut subscriber = builder().subscribe(&topic).unwrap();
        assert!(builder().subscribe(&topic).is_err());
        // It reads nothing: a wait for room takes it out.
        for byte in 0..=255 {
            publisher.publish(&Bytes(vec![byte; 64])).unwrap();
        }
        assert_eq!(publisher.stats().evicted, 1);
        drop(publisher);
        while subscriber.try_recv().is_ok() {}
        assert!(matches!(subscriber.try_recv(), Err(RecvError::Closed(_))));
        assert!(matches!(subscriber.try_recv(), Err(RecvError::Closed(_))));
        drop(subscriber);
    });
    let count = |message: &str| messages.iter().filter(|(m, _)| m == message).count();
    assert_eq!(count("subscribed"), 1, "{messages:?}");
    assert_eq!(count("subscription taken"), 1, "{messages:?}");
    assert_eq!(count("subscription refused"), 1, "{messages:?}");
    assert_eq!(count("subscription taken out"), 1, "{messages:?}");
    assert_eq!(count("topic ended"), 1, "{messages:?}");
    assert_eq!(count("subscription closed"), 1, "{messages:?}");
    // 256 publishes, and no event of their own.
    assert!(messages.len() < 12, "{messages:?}");
}

#[test]
fn a_dropped_subscription_ends_with_its_reason() {
    let messages = collected(|| {
        let publisher = Publisher::<Bytes>::events().build().unwrap();
        let subscriber = Subscriber::<Bytes>::builder()
            .subscribe(&publisher.as_binder())
            .unwrap();
        drop(subscriber);
        assert_eq!(publisher.subscribers().count, 0);
    });
    let ended = (String::from("subscription ended"), String::from("closed"));
    assert!(messages.contains(&ended), "{messages:?}");
}

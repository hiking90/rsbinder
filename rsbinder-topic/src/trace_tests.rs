// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `tracing` events of a subscription's life, collected on the thread that makes them:
//! within one process the subscription call and the publish run on the caller's.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::{Event, Metadata};

use crate::{Bytes, Publisher, RecvError, Start, Subscriber};

/// The message of each event of this crate's target, in order.
#[derive(Clone, Default)]
struct Collect(Arc<Mutex<Vec<String>>>);

struct Message<'a>(&'a mut String);

impl Visit for Message<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            *self.0 = format!("{value:?}");
        }
    }
}

impl tracing::Subscriber for Collect {
    fn enabled(&self, metadata: &Metadata<'_>) -> bool {
        metadata.target() == "rsbinder_topic"
    }
    fn new_span(&self, _: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }
    fn record(&self, _: &Id, _: &Record<'_>) {}
    fn record_follows_from(&self, _: &Id, _: &Id) {}
    fn event(&self, event: &Event<'_>) {
        let mut message = String::new();
        event.record(&mut Message(&mut message));
        self.0.lock().unwrap().push(message);
    }
    fn enter(&self, _: &Id) {}
    fn exit(&self, _: &Id) {}
}

#[test]
fn a_subscriptions_life_is_an_event_at_each_step_and_none_per_record() {
    let collect = Collect::default();
    tracing::subscriber::with_default(collect.clone(), || {
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
    let messages = collect.0.lock().unwrap().clone();
    let count = |message: &str| messages.iter().filter(|m| *m == message).count();
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
    let collect = Collect::default();
    tracing::subscriber::with_default(collect.clone(), || {
        let publisher = Publisher::<Bytes>::events().build().unwrap();
        let subscriber = Subscriber::<Bytes>::builder()
            .subscribe(&publisher.as_binder())
            .unwrap();
        drop(subscriber);
        assert_eq!(publisher.subscribers().count, 0);
    });
    let messages = collect.0.lock().unwrap().clone();
    assert!(
        messages.iter().any(|m| m == "subscription ended"),
        "{messages:?}"
    );
}

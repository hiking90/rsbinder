// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic registered with the service manager and found there. With `rsb_hub` running:
//!
//! ```text
//! cargo run -p rsbinder-topic --example topic_readings -- publish
//! cargo run -p rsbinder-topic --example topic_readings -- subscribe
//! ```
//!
//! The publisher publishes a hundred readings, one every 100 ms, then drops the topic; each
//! subscriber prints what it receives until the topic's end.

use std::time::Duration;

use rsbinder::{hub, ProcessState};
use rsbinder_topic::{Closed, Publisher, PublisherPolicy, RecvError, Subscriber, SubscriberPolicy};

// ANCHOR: payload
#[derive(rsbinder::Parcelable, Debug, Default)]
#[parcelable(descriptor = "example.topic.Reading")]
struct Reading {
    seq: i64,
    celsius: f64,
}
// ANCHOR_END: payload

const NAME: &str = "example.topic.readings";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: init
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    // ANCHOR_END: init
    match std::env::args().nth(1).as_deref() {
        Some("publish") => publish(),
        Some("subscribe") => subscribe(),
        _ => Err("usage: topic_readings publish|subscribe".into()),
    }
}

// ANCHOR: publish
fn publish() -> Result<(), Box<dyn std::error::Error>> {
    let publisher = Publisher::<Reading>::new(&PublisherPolicy::default())?;
    hub::add_service(NAME, publisher.as_binder())?;
    for seq in 0..100 {
        let celsius = 20.0 + (seq % 10) as f64 / 10.0;
        publisher.publish(&Reading { seq, celsius })?;
        std::thread::sleep(Duration::from_millis(100));
    }
    // Dropping the publisher writes the topic's end record.
    Ok(())
}
// ANCHOR_END: publish

// ANCHOR: subscribe
fn subscribe() -> Result<(), Box<dyn std::error::Error>> {
    let topic = hub::check_service(NAME).ok_or("no publisher has registered the topic")?;
    let mut subscriber = Subscriber::<Reading>::request(&topic, &SubscriberPolicy::default())?;
    loop {
        match subscriber.recv() {
            Ok(reading) => println!("{reading:?}"),
            Err(RecvError::Lagged(lost)) => println!("{lost} readings were overwritten unread"),
            Err(RecvError::Decode(code)) => println!("a reading failed to decode: {code}"),
            Err(RecvError::Closed(Closed::End)) => return Ok(()),
            Err(why) => return Err(why.into()),
        }
    }
}
// ANCHOR_END: subscribe

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic found by name, the publisher and the subscriber started in either order. With
//! `rsb_hub` running:
//!
//! ```text
//! cargo run -p rsbinder-topic --example topic_named -- subscribe
//! cargo run -p rsbinder-topic --example topic_named -- publish
//! ```
//!
//! The subscriber connects to whichever publisher holds the name; run `publish` again after it
//! ends and the subscriber goes on with the new one.

use std::time::Duration;

use rsbinder::ProcessState;
use rsbinder_topic::{
    Bytes, Closed, NamedSubscriber, Publisher, PublisherPolicy, RecvError, SubscriberPolicy,
};

const NAME: &str = "example.topic.counter";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    match std::env::args().nth(1).as_deref() {
        Some("publish") => publish(),
        Some("subscribe") => subscribe(),
        _ => Err("usage: topic_named publish|subscribe".into()),
    }
}

// ANCHOR: advertise
fn publish() -> Result<(), Box<dyn std::error::Error>> {
    let publisher = Publisher::<Bytes>::advertise(NAME, &PublisherPolicy::default())?;
    for n in 0..50u32 {
        publisher.publish(&Bytes(n.to_le_bytes().to_vec()))?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}
// ANCHOR_END: advertise

// ANCHOR: named
fn subscribe() -> Result<(), Box<dyn std::error::Error>> {
    let policy = SubscriberPolicy::default();
    let mut subscriber = NamedSubscriber::<Bytes>::subscribe(NAME, &policy)?;
    loop {
        match subscriber.recv() {
            Ok(Bytes(value)) => println!("{value:?}"),
            Err(RecvError::Lagged(lost)) => println!("{lost} values were overwritten unread"),
            // The publisher ended, died or lost the name: the subscriber waits for the next one.
            Err(RecvError::Disconnected(why)) => println!("{why}"),
            Err(RecvError::Closed(Closed::Evicted)) => return Err("taken out by the topic".into()),
            Err(why) => return Err(why.into()),
        }
    }
}
// ANCHOR_END: named

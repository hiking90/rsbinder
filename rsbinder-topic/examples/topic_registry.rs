// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic named in `rsb_hub`'s registry of topic names, apart from the service names. With an
//! `rsb_hub` built with its `topic` feature, on a configuration that lets this user advertise,
//! subscribe to and list the name (the test suite's two permissive files do):
//!
//! ```text
//! cargo build -p rsbinder-tools --features topic --bin rsb_hub
//! mkdir -m 755 hub.d && cp tests/policy/permissive.toml tests/policy/topic/permissive-topic.toml hub.d/
//! target/debug/rsb_hub --config hub.d &
//! cargo run -p rsbinder-topic --example topic_registry -- subscribe
//! cargo run -p rsbinder-topic --example topic_registry -- publish
//! cargo run -p rsbinder-topic --example topic_registry -- list
//! ```
//!
//! The publisher publishes fifty counts, one every 100 ms, and gives the name back as it ends;
//! the subscriber goes on with whichever publisher holds the name next.

use std::time::Duration;

use rsbinder::ProcessState;
use rsbinder_topic::registry::RegistryResolver;
use rsbinder_topic::{
    Bytes, NamedSubscriber, Publisher, PublisherPolicy, RecvError, SubscriberPolicy,
};

const NAME: &str = "example.topic.counter";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    // ANCHOR: find
    let registry = RegistryResolver::find()?;
    // ANCHOR_END: find
    match std::env::args().nth(1).as_deref() {
        Some("publish") => publish(&registry),
        Some("subscribe") => subscribe(&registry),
        Some("list") => list(&registry),
        _ => Err("usage: topic_registry publish|subscribe|list".into()),
    }
}

// ANCHOR: advertise
fn publish(registry: &RegistryResolver) -> Result<(), Box<dyn std::error::Error>> {
    let publisher =
        Publisher::<Bytes>::advertise_with(NAME, &PublisherPolicy::default(), registry)?;
    for n in 0..50u32 {
        publisher.publish(&Bytes(n.to_le_bytes().to_vec()))?;
        std::thread::sleep(Duration::from_millis(100));
    }
    // Dropping the publisher takes the name back, then writes the topic's end record.
    Ok(())
}
// ANCHOR_END: advertise

// ANCHOR: subscribe
fn subscribe(registry: &RegistryResolver) -> Result<(), Box<dyn std::error::Error>> {
    let policy = SubscriberPolicy::default();
    let mut subscriber = NamedSubscriber::<Bytes>::subscribe_with(NAME, &policy, registry)?;
    loop {
        match subscriber.recv() {
            Ok(Bytes(value)) => println!("{value:?}"),
            Err(RecvError::Disconnected(why)) => println!("{why}"),
            Err(why) => return Err(why.into()),
        }
    }
}
// ANCHOR_END: subscribe

// ANCHOR: list
fn list(registry: &RegistryResolver) -> Result<(), Box<dyn std::error::Error>> {
    for topic in registry.list("example.")? {
        println!(
            "{} uid={} pid={} {} ms, {} watcher(s)",
            topic.name, topic.uid, topic.pid, topic.ageMillis, topic.watchers
        );
    }
    Ok(())
}
// ANCHOR_END: list

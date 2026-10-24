// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Values written straight into the topic's pool and received where they lie. With `rsb_hub`
//! running:
//!
//! ```text
//! cargo run -p rsbinder-topic --example topic_frames -- publish
//! cargo run -p rsbinder-topic --example topic_frames -- subscribe
//! cargo run -p rsbinder-topic --example topic_frames -- subscribe-trusted
//! ```
//!
//! The publisher lends a 300 KiB frame thirty times a second for ten seconds. `subscribe` copies
//! each one out; `subscribe-trusted` vouches for the publisher and reads each in place.

use std::time::Duration;

use rsbinder::{hub, ProcessState, SIBinder};
use rsbinder_topic::{
    Bytes, Closed, PoolPolicy, Publisher, PublisherPolicy, RecvError, Subscriber, SubscriberPolicy,
};

const NAME: &str = "example.topic.frames";
const FRAME: usize = 640 * 480;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    match std::env::args().nth(1).as_deref() {
        Some("publish") => publish(),
        Some("subscribe") => subscribe(&topic()?),
        Some("subscribe-trusted") => subscribe_trusted(&topic()?),
        _ => Err("usage: topic_frames publish|subscribe|subscribe-trusted".into()),
    }
}

fn topic() -> Result<SIBinder, Box<dyn std::error::Error>> {
    Ok(hub::check_service(NAME).ok_or("no publisher has registered the topic")?)
}

// ANCHOR: lend
fn publish() -> Result<(), Box<dyn std::error::Error>> {
    let policy = PublisherPolicy {
        max_subscribers: 4,
        pool: Some(PoolPolicy {
            slot_bytes: FRAME,
            ..PoolPolicy::default()
        }),
        ..PublisherPolicy::default()
    };
    let publisher = Publisher::<Bytes>::new(&policy)?;
    hub::add_service(NAME, publisher.as_binder())?;
    for n in 0..300u32 {
        let mut frame = publisher.loan_slice(FRAME)?;
        frame.fill(n as u8);
        frame.publish()?;
        std::thread::sleep(Duration::from_millis(33));
    }
    Ok(())
}
// ANCHOR_END: lend

// ANCHOR: sample
fn subscribe(topic: &SIBinder) -> Result<(), Box<dyn std::error::Error>> {
    let mut subscriber = Subscriber::<Bytes>::request(topic, &SubscriberPolicy::default())?;
    let mut frame = vec![0; FRAME];
    loop {
        match subscriber.recv_sample() {
            Ok(sample) if sample.len() == FRAME => {
                // Atomic loads: what a publisher that breaks the protocol can do is wrong bytes.
                sample.copy_to(&mut frame);
                println!("frame {}", frame[0]);
            }
            Ok(sample) => println!("a value of {} bytes", sample.len()),
            Err(RecvError::Lagged(lost)) => println!("{lost} frames were lost"),
            Err(RecvError::Closed(Closed::End)) => return Ok(()),
            Err(why) => return Err(why.into()),
        }
    }
}
// ANCHOR_END: sample

// ANCHOR: trusted
fn subscribe_trusted(topic: &SIBinder) -> Result<(), Box<dyn std::error::Error>> {
    let subscriber = Subscriber::<Bytes>::request(topic, &SubscriberPolicy::default())?;
    // SAFETY: this system's own rsbinder-topic `Publisher`, which writes no pinned slot.
    let mut subscriber = unsafe { subscriber.trust_publisher() };
    loop {
        match subscriber.recv_sample() {
            // The sample derefs to the slot's bytes, read in place.
            Ok(sample) => {
                let sum: u64 = sample.iter().map(|&byte| u64::from(byte)).sum();
                println!("{} bytes, sum {sum}", sample.len());
            }
            Err(RecvError::Lagged(lost)) => println!("{lost} frames were lost"),
            Err(RecvError::Closed(Closed::End)) => return Ok(()),
            Err(why) => return Err(why.into()),
        }
    }
}
// ANCHOR_END: trusted

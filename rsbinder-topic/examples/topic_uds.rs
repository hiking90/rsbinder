// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A topic over a Unix domain socket, with no binder device: at a path, or found by name through
//! a hub. The book's Topics chapter quotes it.
//!
//! ```text
//! cargo run -p rsbinder-topic --features uds --example topic_uds -- publish /tmp/topic.sock &
//! cargo run -p rsbinder-topic --features uds --example topic_uds -- subscribe /tmp/topic.sock
//! ```
//!
//! Through a hub (`hub.toml`, owned by you and writable by nobody else, says who may do what):
//!
//! ```text
//! cargo run -p rsbinder-tools --features topic,rpc --bin rsb_hub -- \
//!     --topic-socket "$XDG_RUNTIME_DIR/rsbinder/topic.sock" --config hub.toml &
//! cargo run -p rsbinder-topic --features uds --example topic_uds -- \
//!     hub-publish "$XDG_RUNTIME_DIR/rsbinder/topic.sock" &
//! cargo run -p rsbinder-topic --features uds --example topic_uds -- \
//!     hub-subscribe "$XDG_RUNTIME_DIR/rsbinder/topic.sock"
//! ```

#[cfg(any(target_os = "linux", target_os = "android"))]
use std::path::PathBuf;
#[cfg(any(target_os = "linux", target_os = "android"))]
use std::time::Duration;

#[cfg(any(target_os = "linux", target_os = "android"))]
use rsbinder::rpc::RpcServer;
#[cfg(any(target_os = "linux", target_os = "android"))]
use rsbinder_topic::registry::uds::{ResolverOptions, UdsResolver};
#[cfg(any(target_os = "linux", target_os = "android"))]
use rsbinder_topic::uds::{self, Endpoint, Options};
#[cfg(any(target_os = "linux", target_os = "android"))]
use rsbinder_topic::{
    Bytes, NamedSubscriber, Publisher, PublisherPolicy, RecvError, SubscriberPolicy,
};

#[cfg(any(target_os = "linux", target_os = "android"))]
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let [_, role, socket] = args.as_slice() else {
        return Err("usage: topic_uds publish|subscribe|hub-publish|hub-subscribe SOCKET".into());
    };
    let path = PathBuf::from(socket);
    match role.as_str() {
        "publish" => publish(path),
        "subscribe" => subscribe(path),
        "hub-publish" => hub_publish(path),
        "hub-subscribe" => hub_subscribe(path),
        other => Err(format!("unknown role {other}").into()),
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn publish(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: serve
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default())?;
    let server = RpcServer::setup_unix_server(path)?;
    let serving = uds::serve(&server, publisher.as_binder())?;
    let mut published = Ok(());
    for n in 0u64.. {
        published = publisher.publish(&Bytes(n.to_le_bytes().to_vec()));
        if published.is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // In this order: subscribers read the topic's end before the server ends their sessions.
    drop(publisher);
    drop(serving);
    // ANCHOR_END: serve
    published?;
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn subscribe(path: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: subscribe
    let options = Options {
        // The publisher's uid, by SO_PEERCRED of the connection: nobody else is listened to.
        expect_uid: Some(own_uid()),
        ..Options::default()
    };
    let policy = SubscriberPolicy::default();
    let mut subscriber = uds::subscribe_with::<Bytes>(Endpoint::Path(path), &policy, &options)?;
    loop {
        match subscriber.recv() {
            Ok(Bytes(value)) => println!("{value:?}"),
            Err(RecvError::Closed(closed)) => {
                println!("ended: {closed:?}");
                return Ok(());
            }
            Err(other) => println!("{other}"),
        }
    }
    // ANCHOR_END: subscribe
}

/// This process's uid, which a publisher started by the same user has too.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn own_uid() -> u32 {
    std::fs::metadata("/proc/self").map_or(0, |proc| {
        use std::os::unix::fs::MetadataExt;
        proc.uid()
    })
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn hub_publish(hub: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    // ANCHOR: hub
    let resolver = UdsResolver::connect(Endpoint::Path(hub), &ResolverOptions::default())?;
    let publisher = Publisher::<Bytes>::advertise_with(
        "example.readings",
        &PublisherPolicy::default(),
        &resolver,
    )?;
    // ANCHOR_END: hub
    for n in 0u64.. {
        publisher.publish(&Bytes(n.to_le_bytes().to_vec()))?;
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn hub_subscribe(hub: PathBuf) -> Result<(), Box<dyn std::error::Error>> {
    let resolver = UdsResolver::connect(Endpoint::Path(hub), &ResolverOptions::default())?;
    let policy = SubscriberPolicy::default();
    let mut subscriber =
        NamedSubscriber::<Bytes>::subscribe_with("example.readings", &policy, &resolver)?;
    loop {
        match subscriber.recv() {
            Ok(Bytes(value)) => println!("{value:?}"),
            Err(RecvError::Disconnected(why)) => println!("{why}: waiting for the next publisher"),
            Err(other) => return Err(other.to_string().into()),
        }
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn main() {
    eprintln!("topics need Linux or Android");
}

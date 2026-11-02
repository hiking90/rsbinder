// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The topic-over-a-socket probe of `tests/container/ac_topic_uds.sh` (plan 15-pubsub-uds §7).
//!
//! Prints what each step got, so the script, which runs every process under a uid of its own,
//! judges real credentials:
//!
//! ```text
//! topic_uds_probe OP...
//!   publish:PATH:VALUE       a Publisher at a socket at PATH, mode 0666, publishing VALUE
//!   subscribe:PATH[:UID]     uds::subscribe_with, with `expect_uid` when UID is given
//!   recv                     a value of the last subscriber
//!   say:TEXT  wait:FILE  sleep:MS  hold[:SECS]
//! ```
//!
//! A step prints `RESULT <op> <arg> <outcome>`, the outcome `OK`, or how it was refused:
//! `SECURITY`, `EACCES`, `ERR:<status>`.

use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::time::Duration;

use rsbinder::rpc::{RpcServer, RpcServerHandle};
use rsbinder::{ExceptionCode, Status, StatusCode};
use rsbinder_topic::uds::{self, Endpoint, Options};
use rsbinder_topic::{Bytes, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy};

fn outcome<T>(result: &Result<T, Status>) -> String {
    match result {
        Ok(_) => "OK".to_owned(),
        Err(status) if status.exception_code() == ExceptionCode::Security => "SECURITY".to_owned(),
        Err(status) if status.transaction_error() == StatusCode::Errno(-libc::EACCES) => {
            "EACCES".to_owned()
        }
        Err(status) => format!("ERR:{status}"),
    }
}

fn result(op: &str, arg: &str, outcome: &str) {
    println!("RESULT {op} {arg} {outcome}");
}

/// What the steps made, kept until the process ends.
#[derive(Default)]
struct Held {
    publishers: Vec<(Publisher<Bytes>, RpcServerHandle)>,
    subscribers: Vec<Subscriber<Bytes>>,
}

fn publish(held: &Mutex<Held>, path: &str, value: &str) {
    let server = RpcServer::setup_unix_server(path).expect("a socket");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o666)).expect("a mode");
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).expect("a publisher");
    let handle = uds::serve(&server, publisher.as_binder()).expect("a server");
    publisher
        .publish(&Bytes(value.as_bytes().to_vec()))
        .expect("a record");
    held.lock().unwrap().publishers.push((publisher, handle));
    result("publish", path, "OK");
}

fn subscribe(held: &Mutex<Held>, path: &str, expect: Option<&str>) {
    let options = Options {
        expect_uid: expect.map(|uid| uid.parse().expect("a uid")),
        ..Options::default()
    };
    let policy = SubscriberPolicy {
        start: rsbinder_topic::Start::Last,
        ..SubscriberPolicy::default()
    };
    let endpoint = Endpoint::Path(path.into());
    let subscriber = uds::subscribe_with::<Bytes>(endpoint, &policy, &options);
    result("subscribe", path, &outcome(&subscriber));
    if let Ok(subscriber) = subscriber {
        held.lock().unwrap().subscribers.push(subscriber);
    }
}

fn recv(held: &Mutex<Held>) {
    let mut held = held.lock().unwrap();
    let subscriber = held.subscribers.last_mut().expect("a subscriber");
    match subscriber.recv_timeout(Duration::from_secs(10)) {
        Ok(Bytes(value)) => result("recv", &String::from_utf8_lossy(&value), "OK"),
        Err(why) => result("recv", "-", &format!("ERR:{why}")),
    }
}

fn main() {
    env_logger::init();
    let held = Mutex::new(Held::default());
    for op in std::env::args().skip(1) {
        let mut parts = op.split(':');
        let name = parts.next().unwrap_or_default();
        let rest: Vec<&str> = parts.collect();
        match (name, rest.as_slice()) {
            ("publish", [path, value]) => publish(&held, path, value),
            ("subscribe", [path]) => subscribe(&held, path, None),
            ("subscribe", [path, uid]) => subscribe(&held, path, Some(uid)),
            ("recv", []) => recv(&held),
            ("say", [text]) => println!("{text}"),
            ("sleep", [ms]) => {
                std::thread::sleep(Duration::from_millis(ms.parse().expect("a time")))
            }
            ("wait", [file]) => {
                while !std::path::Path::new(file).exists() {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
            ("hold", []) => std::thread::sleep(Duration::from_secs(3600)),
            ("hold", [secs]) => {
                std::thread::sleep(Duration::from_secs(secs.parse().expect("a time")))
            }
            _ => panic!("unknown op {op}"),
        }
    }
}

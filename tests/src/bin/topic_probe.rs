// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The topic registry probe of `tests/scripts/run_topic_registry_ac.sh` (plan 15 P5-3).
//!
//! Drives the registry `rsb_hub` serves with its `topic` feature over real kernel binder, and
//! prints what each step got, so the script judges a real hub's answers:
//!
//! ```text
//! topic_probe [--mmap BYTES] OP...
//!   find                     RegistryResolver::find
//!   publish:NAME:VALUE       a Publisher of NAME, publishing VALUE every 50 ms
//!   lookup:NAME              ITopicRegistry::lookup
//!   list:PREFIX              ITopicRegistry::list, a `LISTED` line per name
//!   flood:NAME:N             N advertises of NAME, each a new topic that refuses subscribers
//!   fill:PREFIX:N            one such advertise of each of PREFIX0 .. PREFIX<N-1>
//!   watchmany:PREFIX:N       a watch of each of PREFIX0 .. PREFIX<N-1>, told nothing
//!   subscribe:NAME           a NamedSubscriber (`Start::Last`), a `GOT` line per new value
//!   addreserved              addService of the registry's own name
//!   capture:NAME             advertise NAME as a topic that keeps the first subscriber's token
//!   relay:CALL:TARGET        hand the kept token to the hub under TARGET: CALL is `add`
//!                            (addService, tryUnregisterService), `notify` (registerFor- and
//!                            unregisterForNotifications), `clientcb` (addService of this
//!                            process's binder, registerClientCallback), `advertise` (and
//!                            withdraw) or `watch` (and unwatch)
//!   pool                     start the binder thread pool
//!   say:TEXT  wait:FILE  sleep:MS  hold[:SECS]
//! ```
//!
//! A step prints `RESULT <op> <arg> <outcome>`, and a call `TIME <op> <arg> <ms>` too, the
//! longest one for `flood`. The outcome is `OK`, `FOUND` or `NULL`, or how the call was refused:
//! `SECURITY`, `TAKEN`, `LIMIT`, `NOTFOUND`, `ERR:<status>`. Every step runs in this one
//! process, which keeps what it made (publishers, subscribers, watches) until it ends.
//!
//! Without `--mmap` the process starts its binder thread pool at once; with it, it maps that
//! many bytes to receive in and starts none until `pool`, so the notices of its watches stay
//! in that mapping, unread, and fill it (AC-15.5.12).
//!
//! `capture` and `relay` hand the hub a binder of another process (plan 16 AC-16.6): a
//! subscriber's token goes to the publisher only, so the hub has never held it until the first
//! `relay`; the later ones pass the handle it then holds through typed casts. `relay` times
//! the call of CALL that hands the token over; its outcome is that call's, or the setup call's
//! (`clientcb`'s addService) if that failed first.

use std::any::Any;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use rsbinder::{
    hub, BinderResult, ExceptionCode, Interface, ProcessState, SIBinder, Status, StatusCode,
    Strong, DEFAULT_BINDER_PATH, DEFAULT_MAX_BINDER_THREADS,
};
use rsbinder_topic::aidl::ITopic::{BnTopic, ITopic, ERROR_CLOSED};
use rsbinder_topic::aidl::ITopicRegistry::{
    ITopicRegistry, ERROR_LIMIT, ERROR_TAKEN, SERVICE_NAME,
};
use rsbinder_topic::aidl::ITopicWatcher::ITopicWatcher;
use rsbinder_topic::aidl::SubscribeRequest::SubscribeRequest;
use rsbinder_topic::aidl::TopicEndpoint::TopicEndpoint;
use rsbinder_topic::registry::RegistryResolver;
use rsbinder_topic::{
    Bytes, NamedSubscriber, Publisher, PublisherPolicy, RecvError, Resolver, Start,
    SubscriberPolicy,
};

/// A topic that refuses every subscriber as ended: the many advertises of a flood make no ring.
struct Ended;

impl Interface for Ended {}

impl ITopic for Ended {
    fn r#subscribe(&self, _request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        Err(Status::new_service_specific_error(ERROR_CLOSED, None))
    }
}

/// The first token a [`Capture`] topic was handed.
static CAPTURED: Mutex<Option<SIBinder>> = Mutex::new(None);

/// A topic that keeps the first subscriber's token and refuses every subscriber as ended.
struct Capture;

impl Interface for Capture {}

impl ITopic for Capture {
    fn r#subscribe(&self, request: &SubscribeRequest) -> BinderResult<TopicEndpoint> {
        let mut captured = CAPTURED.lock().expect("lock poisoned");
        if let (None, Some(token)) = (captured.as_ref(), &request.token) {
            *captured = Some(token.clone());
            println!("SAY captured");
        }
        Err(Status::new_service_specific_error(ERROR_CLOSED, None))
    }
}

/// A callback nothing calls: the hub's are oneway, and the stopped process would not answer.
struct Silent;

impl Interface for Silent {}

impl hub::IClientCallback for Silent {
    fn r#onClients(&self, _: &SIBinder, _: bool) -> BinderResult<()> {
        Ok(())
    }
}

/// Hands the kept token to the hub by `call`, timed; unchecked casts ask its process nothing.
fn relay(
    call: &str,
    target: &str,
    kept: &mut Vec<Box<dyn Any>>,
) -> BinderResult<(BinderResult<()>, Duration)> {
    let token = CAPTURED.lock().expect("lock poisoned").clone();
    let token = token.ok_or(StatusCode::NameNotFound)?;
    let timed = |call: &mut dyn FnMut() -> BinderResult<()>| {
        let began = Instant::now();
        let got = call();
        (got, began.elapsed())
    };
    match call {
        "add" => {
            let got = timed(&mut || hub::add_service(target, token.clone()));
            // Refused while the hub counts a client, as AOSP's: the registration stays.
            let _ = hub::try_unregister_service(target, &token);
            Ok(got)
        }
        "notify" => {
            let callback: Strong<dyn hub::IServiceCallback> = token.into_interface()?;
            let got = timed(&mut || Ok(hub::register_for_notifications(target, &callback)?));
            let _ = hub::unregister_for_notifications(target, &callback);
            Ok(got)
        }
        "clientcb" => {
            let own = hub::BnClientCallback::new_binder(Silent).as_binder();
            hub::add_service(target, own.clone())?;
            kept.push(Box::new(own.clone()));
            let callback: Strong<dyn hub::IClientCallback> = token.into_interface()?;
            Ok(timed(&mut || {
                Ok(hub::register_client_callback(target, &own, &callback)?)
            }))
        }
        "advertise" => {
            let registry = registry()?;
            let topic: Strong<dyn ITopic> = token.into_interface()?;
            let got = timed(&mut || registry.r#advertise(target, &topic));
            let _ = registry.r#withdraw(target, &topic);
            Ok(got)
        }
        "watch" => {
            let registry = registry()?;
            let watcher: Strong<dyn ITopicWatcher> = token.into_interface()?;
            let got = timed(&mut || registry.r#watch(target, &watcher).map(drop));
            let _ = registry.r#unwatch(target, &watcher);
            Ok(got)
        }
        _ => panic!("unknown relay call {call:?}"),
    }
}

fn outcome(status: &Status) -> String {
    match status.exception_code() {
        ExceptionCode::Security => "SECURITY".into(),
        ExceptionCode::ServiceSpecific if status.service_specific_error() == ERROR_TAKEN => {
            "TAKEN".into()
        }
        ExceptionCode::ServiceSpecific if status.service_specific_error() == ERROR_LIMIT => {
            "LIMIT".into()
        }
        _ if status.transaction_error() == StatusCode::NameNotFound => "NOTFOUND".into(),
        _ => format!("ERR:{status}").replace(char::is_whitespace, "_"),
    }
}

fn result(op: &str, arg: &str, got: &str) {
    println!("RESULT {op} {arg} {got}");
}

fn time(op: &str, arg: &str, took: Duration) {
    println!("TIME {op} {arg} {}", took.as_millis());
}

fn registry() -> BinderResult<Strong<dyn ITopicRegistry>> {
    let binder = hub::check_service(SERVICE_NAME).ok_or(StatusCode::NameNotFound)?;
    Ok(binder.into_interface()?)
}

fn ended() -> SIBinder {
    BnTopic::new_binder(Ended).as_binder()
}

/// `PREFIX0` .. `PREFIX<N-1>`.
fn names(arg: &str) -> Vec<String> {
    let (prefix, count) = arg.rsplit_once(':').expect("PREFIX:N");
    let count: usize = count.parse().expect("a count");
    (0..count).map(|i| format!("{prefix}{i}")).collect()
}

/// Ok when every name took a topic; the first refusal otherwise.
fn advertise_ended(names: impl IntoIterator<Item = String>) -> (BinderResult<()>, Duration) {
    let mut longest = Duration::ZERO;
    let registry = match registry() {
        Ok(registry) => registry,
        Err(status) => return (Err(status), longest),
    };
    for name in names {
        let topic: Strong<dyn ITopic> = ended().into_interface().expect("a topic");
        let began = Instant::now();
        let advertised = registry.r#advertise(&name, &topic);
        longest = longest.max(began.elapsed());
        if let Err(status) = advertised {
            return (Err(status), longest);
        }
    }
    (Ok(()), longest)
}

fn resolver() -> BinderResult<RegistryResolver> {
    RegistryResolver::find()
}

fn subscribe(name: &str, kept: &mut Vec<Box<dyn Any>>) -> BinderResult<()> {
    let policy = SubscriberPolicy {
        start: Start::Last,
        ..SubscriberPolicy::default()
    };
    let mut subscriber = NamedSubscriber::<Bytes>::subscribe_with(name, &policy, &resolver()?)?;
    let name = name.to_owned();
    kept.push(Box::new(std::thread::spawn(move || {
        let mut last = None;
        loop {
            match subscriber.recv() {
                Ok(Bytes(value)) if last.as_ref() != Some(&value) => {
                    println!("GOT {name} {}", String::from_utf8_lossy(&value));
                    last = Some(value);
                }
                Ok(_) | Err(RecvError::Lagged(_)) | Err(RecvError::Disconnected(_)) => {}
                Err(why) => {
                    println!("RECV {name} {why}");
                    return;
                }
            }
        }
    })));
    Ok(())
}

fn publish(name: &str, value: &str, kept: &mut Vec<Box<dyn Any>>) -> BinderResult<()> {
    let publisher =
        Publisher::<Bytes>::advertise_with(name, &PublisherPolicy::default(), &resolver()?)?;
    let value = Bytes(value.as_bytes().to_vec());
    kept.push(Box::new(std::thread::spawn(move || loop {
        let _ = publisher.publish(&value);
        std::thread::sleep(Duration::from_millis(50));
    })));
    Ok(())
}

fn step(op: &str, arg: &str, kept: &mut Vec<Box<dyn Any>>) {
    let done = |got: BinderResult<()>| match got {
        Ok(()) => result(op, arg, "OK"),
        Err(status) => result(op, arg, &outcome(&status)),
    };
    match op {
        "find" => done(resolver().map(drop)),
        "publish" => {
            let (name, value) = arg.split_once(':').expect("NAME:VALUE");
            let began = Instant::now();
            let got = publish(name, value, kept);
            time(op, name, began.elapsed());
            match got {
                Ok(()) => result(op, name, "OK"),
                Err(status) => result(op, name, &outcome(&status)),
            }
        }
        "lookup" => {
            let began = Instant::now();
            let got = registry().and_then(|registry| registry.r#lookup(arg));
            time(op, arg, began.elapsed());
            match got {
                Ok(found) if found.topic.is_some() => result(op, arg, "FOUND"),
                Ok(_) => result(op, arg, "NULL"),
                Err(status) => result(op, arg, &outcome(&status)),
            }
        }
        "list" => {
            let began = Instant::now();
            let got = registry().and_then(|registry| registry.r#list(arg));
            time(op, arg, began.elapsed());
            match got {
                Ok(listed) => {
                    for info in &listed {
                        println!("LISTED {} {} {}", info.name, info.uid, info.watchers);
                    }
                    result(op, arg, "OK");
                }
                Err(status) => result(op, arg, &outcome(&status)),
            }
        }
        "flood" => {
            let (name, count) = arg.split_once(':').expect("NAME:N");
            let count: usize = count.parse().expect("a count");
            let (got, longest) = advertise_ended((0..count).map(|_| name.to_owned()));
            time(op, name, longest);
            match got {
                Ok(()) => result(op, name, "OK"),
                Err(status) => result(op, name, &outcome(&status)),
            }
        }
        "fill" => {
            let (got, _) = advertise_ended(names(arg));
            done(got);
        }
        "watchmany" => done((|| {
            let resolver = resolver()?;
            for name in names(arg) {
                kept.push(Box::new(resolver.watch(&name, Box::new(|_| {}))?));
            }
            Ok(())
        })()),
        "subscribe" => done(subscribe(arg, kept)),
        "addreserved" => done(hub::add_service(SERVICE_NAME, ended())),
        "capture" => done((|| {
            let topic: Strong<dyn ITopic> = BnTopic::new_binder(Capture);
            registry()?.r#advertise(arg, &topic)?;
            kept.push(Box::new(topic));
            Ok(())
        })()),
        "relay" => {
            let (call, target) = arg.split_once(':').expect("CALL:TARGET");
            match relay(call, target, kept) {
                Ok((got, took)) => {
                    time(op, call, took);
                    match got {
                        Ok(()) => result(op, call, "OK"),
                        Err(status) => result(op, call, &outcome(&status)),
                    }
                }
                Err(status) => result(op, call, &outcome(&status)),
            }
        }
        "pool" => {
            ProcessState::start_thread_pool();
            result(op, "-", "OK");
        }
        "say" => println!("SAY {arg}"),
        "wait" => {
            let deadline = Instant::now() + Duration::from_secs(60);
            while !std::path::Path::new(arg).exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(10));
            }
            let got = if std::path::Path::new(arg).exists() {
                "OK"
            } else {
                "TIMEOUT"
            };
            result(op, arg, got);
        }
        "sleep" => std::thread::sleep(Duration::from_millis(arg.parse().expect("ms"))),
        "hold" => match arg.parse::<u64>() {
            Ok(secs) => std::thread::sleep(Duration::from_secs(secs)),
            Err(_) => loop {
                std::thread::park();
            },
        },
        _ => panic!("unknown step {op:?}"),
    }
}

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let mmap = match args.first().map(String::as_str) {
        Some("--mmap") => {
            let size = args.get(1).expect("--mmap BYTES").parse().expect("a size");
            args.drain(..2);
            Some(size)
        }
        _ => None,
    };
    match mmap {
        Some(size) => {
            ProcessState::init_with_mmap_size(
                DEFAULT_BINDER_PATH,
                DEFAULT_MAX_BINDER_THREADS,
                size,
            )
            .expect("a binder device");
        }
        None => {
            ProcessState::init_default().expect("a binder device");
            ProcessState::start_thread_pool();
        }
    }
    let mut kept: Vec<Box<dyn Any>> = Vec::new();
    for arg in &args {
        let (op, rest) = arg.split_once(':').unwrap_or((arg, ""));
        step(op, rest, &mut kept);
    }
}

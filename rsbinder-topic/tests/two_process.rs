// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A publisher and a subscriber in two processes over the kernel binder (plan 15 AC-15.2.4,
//! 2.13, 2.20, 2.23, 2.44, and the end of a subscription whose publisher died). `#[ignore]`d: they
//! need a binder device and a service manager that lets any name register:
//!
//! ```text
//! target/debug/rsb_hub --config tests/policy/permissive.toml &
//! cargo test -p rsbinder-topic --test two_process -- --ignored --test-threads=1
//! ```
//!
//! The other process is this test binary again, running `child` with `RSBINDER_TOPIC_ROLE` set;
//! it answers on stdout with lines that start with `topic:` and exits when its stdin closes.
//! The publisher's side counts its memfds (fds open on one, mappings of one), so the tests run
//! one at a time.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::fs::File;
use std::io::{BufRead, BufReader, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::sync::Once;
use std::time::{Duration, Instant};

use rsbinder::fmq::shm::{self, CreateOptions, Seals};
use rsbinder::{
    hub, ExceptionCode, Interface, ParcelFileDescriptor, ProcessState, SIBinder, StatusCode,
};
use rsbinder_topic::aidl::ITopic::{ITopic, ERROR_VERSION};
use rsbinder_topic::aidl::SubscribeRequest::SubscribeRequest;
use rsbinder_topic::{
    wait_any, Bytes, Closed, Disconnected, NamedSubscriber, Payload, Publisher, PublisherPolicy,
    RecvError, Subscriber, SubscriberPolicy, Wake,
};

const ROLE: &str = "RSBINDER_TOPIC_ROLE";
const NAME: &str = "RSBINDER_TOPIC_NAME";
const WAIT: Duration = Duration::from_secs(10);

fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        ProcessState::init_default().expect("a binder device");
        ProcessState::start_thread_pool();
    });
}

fn name(tag: &str) -> String {
    format!("rsb.test.topic.{tag}.{}", std::process::id())
}

/// This process's memfds: the fds open on one, and the mappings of one.
fn memfds() -> (usize, usize) {
    let fds = std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy().starts_with("/memfd:"))
        .count();
    let maps = std::fs::read_to_string("/proc/self/maps").unwrap();
    (
        fds,
        maps.lines().filter(|line| line.contains("/memfd:")).count(),
    )
}

/// Poll `done` until it holds or [`WAIT`] passes.
fn eventually(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !done() {
        assert!(Instant::now() < deadline, "{what} within {WAIT:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The other process, killed when dropped.
struct Peer {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: mpsc::Receiver<String>,
}

impl Peer {
    fn spawn(role: &str, name: &str) -> Self {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "child",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(ROLE, role)
            .env(NAME, name)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let (send, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                // Not only at the start: libtest's `test child ... ` has no newline before it.
                if let Some((_, said)) = line.split_once("topic: ") {
                    let _ = send.send(said.to_owned());
                }
            }
        });
        let stdin = child.stdin.take();
        Self {
            child,
            stdin,
            lines,
        }
    }

    /// The next line the peer says.
    fn says(&self) -> String {
        self.lines.recv_timeout(WAIT).expect("the peer to answer")
    }

    fn tell(&mut self, line: &str) {
        writeln!(self.stdin.as_mut().unwrap(), "{line}").unwrap();
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.kill();
    }
}

fn publish(tag: &str) -> (String, Publisher<Bytes>) {
    init();
    let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default()).unwrap();
    let name = name(tag);
    hub::add_service(&name, publisher.as_binder()).unwrap();
    (name, publisher)
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_killed_or_dropped_subscriber_is_taken_out_and_its_page_unmapped() {
    let (name, publisher) = publish("gone");
    let before = memfds();
    let mut peer = Peer::spawn("subscribe", &name);
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    // The control page is mapped and its fd closed.
    assert_eq!(memfds(), (before.0, before.1 + 1));
    peer.kill();
    // The death notification alone, with no publish or count to look at the page.
    eventually("the death to unmap the page", || memfds() == before);
    assert_eq!(publisher.subscriber_count(), 0);

    let mut peer = Peer::spawn("subscribe", &name);
    assert_eq!(peer.says(), "subscribed");
    assert_eq!(publisher.subscriber_count(), 1);
    peer.tell("drop");
    assert_eq!(peer.says(), "dropped");
    // No death: the page says `CLOSED`, and the count takes it out.
    assert_eq!(publisher.subscriber_count(), 0);
    assert_eq!(memfds(), before);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn subscribers_killed_while_they_subscribe_leave_nothing() {
    let (name, publisher) = publish("killed");
    let before = memfds();
    let kills: u32 = std::env::var("RSBINDER_TOPIC_KILLS")
        .ok()
        .and_then(|kills| kills.parse().ok())
        .unwrap_or(1000);
    let mut seed = u64::from(std::process::id()) | 1;
    for _ in 0..kills {
        let mut peer = Peer::spawn("subscribe-loop", &name);
        assert_eq!(peer.says(), "looping");
        // xorshift: a delay up to 2 ms, so that the kill lands anywhere in a subscription.
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        std::thread::sleep(Duration::from_micros(seed % 2000));
        peer.kill();
        // Some of the dropped ones wait for a commit; one keeps the list short.
        publisher.publish(&Bytes(Vec::new())).unwrap();
    }
    // The last kill came after the last publish: its death notification takes it out.
    eventually("every page unmapped", || memfds() == before);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_request_from_another_process_is_refused_for_what_is_wrong_with_it() {
    let (name, publisher) = publish("refused");
    let before = memfds();
    let peer = Peer::spawn("refuse", &name);
    assert_eq!(peer.says(), format!("version: specific {ERROR_VERSION}"));
    assert_eq!(peer.says(), "type: BadType");
    // The topic's own binder: one of the publisher's process, not the caller's.
    assert_eq!(peer.says(), "foreign token: BadValue");
    assert_eq!(peer.says(), "done");
    assert_eq!(publisher.subscriber_count(), 0);
    assert_eq!(memfds(), before);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_reply_the_subscriber_refuses_leaves_its_page_closed() {
    let (name, publisher) = publish("small");
    let before = memfds();
    let peer = Peer::spawn("small-ring", &name);
    assert_eq!(peer.says(), "refused: BadValue");
    // As below, with the reply delivered and refused (a ring above the subscriber's bound).
    assert_eq!(memfds(), (before.0, before.1 + 1));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(memfds(), before);
    assert_eq!(publisher.subscriber_count(), 0);
}

/// Whether the binder driver is the Rust one: its module is loaded.
fn rust_binder() -> bool {
    std::path::Path::new("/sys/module/rust_binder").exists()
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_reply_the_subscriber_cannot_take_leaves_its_page_closed() {
    if rust_binder() {
        // `drivers/android/binder/transaction.rs`: a reply whose fds cannot be installed is
        // dropped, and `BR_FAILED_REPLY` goes only to the sender of a transaction, so the
        // caller waits for ever. The C driver returns `BR_FAILED_REPLY` to it (`binder.c`).
        eprintln!("skipped: the Rust binder driver leaves the caller of a failed reply waiting");
        return;
    }
    let (name, publisher) = publish("nofile");
    let before = memfds();
    let peer = Peer::spawn("nofile", &name);
    // EMFILE: the kernel had no fd number to give the topic memory in the reply.
    assert_eq!(peer.says(), "failed: Errno(-24)");
    // The publisher added the subscription and holds its control page, mapped, though the
    // reply failed; the subscriber, still alive, closed the page, and the next commit takes
    // the subscription out.
    assert_eq!(memfds(), (before.0, before.1 + 1));
    publisher.publish(&Bytes(vec![1])).unwrap();
    assert_eq!(memfds(), before);
    assert_eq!(publisher.subscriber_count(), 0);
}

#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_subscriber_reads_what_a_dead_publisher_committed_then_learns_it_died() {
    init();
    let name = name("died");
    let mut peer = Peer::spawn("publish", &name);
    assert_eq!(peer.says(), "ready");
    let topic = hub::check_service(&name).expect("the publisher's binder");
    let mut subscriber =
        Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
    peer.tell("publish 3");
    assert_eq!(peer.says(), "published");
    peer.kill();
    let mut received = Vec::new();
    eventually("the end of the subscription", || {
        match subscriber.try_recv() {
            Ok(value) => {
                received.push(value);
                false
            }
            Err(RecvError::Empty) => false,
            Err(RecvError::Closed(Closed::PublisherDied)) => true,
            Err(other) => panic!("{other}"),
        }
    });
    assert_eq!(
        received,
        (0..3).map(|byte| Bytes(vec![byte])).collect::<Vec<_>>()
    );
    let died = Err(RecvError::Closed(Closed::PublisherDied));
    assert_eq!(subscriber.try_recv(), died);
    assert_eq!(subscriber.try_latest(), died);
}

/// AC-15.2.20: a subscriber of a name made before its publisher receives from it; after the
/// publisher is killed and started again, one `Disconnected`, then the new one's first record.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_name_subscriber_outlives_a_killed_publisher_and_reads_the_next_from_its_first_record() {
    init();
    let name = name("named");
    let policy = SubscriberPolicy::default();
    let mut subscriber = NamedSubscriber::<Bytes>::subscribe(&name, &policy).unwrap();
    assert!(!subscriber.wait_connected(Duration::from_millis(100)));
    let mut peer = Peer::spawn("advertise", &name);
    assert_eq!(peer.says(), "ready");
    assert!(subscriber.wait_connected(WAIT));
    peer.tell("publish 2");
    assert_eq!(peer.says(), "published");
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![0])));
    assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![1])));
    peer.kill();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Disconnected(Disconnected::Ended(
            Closed::PublisherDied
        )))
    );
    let mut peer = Peer::spawn("advertise", &name);
    assert_eq!(peer.says(), "ready");
    // Published whether or not the subscriber has connected yet.
    peer.tell("publish 3");
    assert_eq!(peer.says(), "published");
    for byte in 0..3 {
        assert_eq!(subscriber.recv_timeout(WAIT), Ok(Bytes(vec![byte])));
    }
    assert_eq!(subscriber.try_recv(), Err(RecvError::Empty));
}

/// AC-15.2.5, 2.11: 8 sleeping subscribers of a killed publisher end within 1 s, 1000 times.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn sleeping_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_of_a_killed_publisher(Wake::Futex);
}

/// The same with the fd wake: the death notice or the socket's hang-up, whichever comes first.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn sleeping_fd_subscribers_learn_within_a_second_that_their_publisher_died() {
    sleepers_of_a_killed_publisher(Wake::Fd);
}

fn sleepers_of_a_killed_publisher(wake: Wake) {
    init();
    let deaths: u32 = std::env::var("RSBINDER_TOPIC_DEATHS")
        .ok()
        .and_then(|deaths| deaths.parse().ok())
        .unwrap_or(1000);
    let policy = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    for death in 0..deaths {
        let name = name(&format!("deaths.{death}"));
        let mut peer = Peer::spawn("publish", &name);
        assert_eq!(peer.says(), "ready");
        let topic = hub::check_service(&name).expect("the publisher's binder");
        let receiving: Vec<_> = (0..8)
            .map(|_| {
                let mut subscriber = Subscriber::<Bytes>::request(&topic, &policy).unwrap();
                std::thread::spawn(move || {
                    let first = subscriber.recv();
                    let killed = subscriber.recv();
                    (first, killed, Instant::now())
                })
            })
            .collect();
        peer.tell("publish 1");
        assert_eq!(peer.says(), "published");
        // Past the spin of 20 µs: each second `recv` sleeps by now.
        std::thread::sleep(Duration::from_millis(5));
        let killed = Instant::now();
        peer.kill();
        eventually("every subscriber is out of its sleep", || {
            receiving.iter().all(|r| r.is_finished())
        });
        for receiving in receiving {
            let (first, then, at) = receiving.join().unwrap();
            assert_eq!(first, Ok(Bytes(vec![0])));
            assert_eq!(then, Err(RecvError::Closed(Closed::PublisherDied)));
            let took = at.saturating_duration_since(killed);
            assert!(took < Duration::from_secs(1), "death {death}: {took:?}");
        }
    }
}

/// AC-15.2.17 across processes: `wait_any` on two publishers; one's death ends its subscription.
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn fd_subscribers_of_two_publishers_wait_on_one_thread_and_learn_of_a_death() {
    init();
    let names = [name("fd.first"), name("fd.second")];
    let mut peers = names.clone().map(|name| Peer::spawn("publish", &name));
    for peer in &peers {
        assert_eq!(peer.says(), "ready");
    }
    let policy = SubscriberPolicy {
        wake: Wake::Fd,
        ..SubscriberPolicy::default()
    };
    let [mut first, mut second] = names.map(|name| {
        let topic = hub::check_service(&name).expect("the publisher's binder");
        Subscriber::<Bytes>::request(&topic, &policy).unwrap()
    });
    for round in 0..200 {
        let from = round % 2;
        peers[from].tell("publish 1");
        let got = loop {
            assert!(wait_any(&mut [&mut first, &mut second], Some(WAIT)).unwrap());
            match (first.try_recv(), second.try_recv()) {
                (Ok(value), Err(RecvError::Empty)) => break (0, value),
                (Err(RecvError::Empty), Ok(value)) => break (1, value),
                (Err(RecvError::Empty), Err(RecvError::Empty)) => {}
                other => panic!("round {round}: {other:?}"),
            }
        };
        assert_eq!(got, (from, Bytes(vec![0])), "round {round}");
        assert_eq!(peers[from].says(), "published");
    }
    let killed = Instant::now();
    peers[0].kill();
    loop {
        assert!(wait_any(&mut [&mut first, &mut second], Some(WAIT)).unwrap());
        match first.try_recv() {
            Err(RecvError::Empty) => {}
            Err(RecvError::Closed(Closed::PublisherDied)) => break,
            other => panic!("{other:?}"),
        }
    }
    assert!(
        killed.elapsed() < Duration::from_secs(1),
        "{:?}",
        killed.elapsed()
    );
    peers[1].tell("publish 1");
    assert!(wait_any(&mut [&mut second], Some(WAIT)).unwrap());
    assert_eq!(second.try_recv(), Ok(Bytes(vec![0])));
}

/// AC-15.2.41: receives after dropped `recv_async`s get each record, then `PublisherDied`.
#[cfg(feature = "tokio")]
#[test]
#[ignore = "needs a binder device and rsb_hub"]
fn a_receive_after_a_dropped_async_receive_learns_that_the_publisher_died() {
    use std::future::Future;
    use std::task::{Context, Poll, Waker};

    init();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .build()
        .unwrap();
    let name = name("joined");
    let mut peer = Peer::spawn("publish", &name);
    assert_eq!(peer.says(), "ready");
    let topic = hub::check_service(&name).expect("the publisher's binder");
    let mut subscriber =
        Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
    let abandon = |subscriber: &mut Subscriber<Bytes>| {
        let _enter = runtime.enter();
        let mut future = std::pin::pin!(subscriber.recv_async());
        let polled = future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()));
        assert!(matches!(polled, Poll::Pending), "nothing to receive yet");
    };
    for round in 0..100 {
        abandon(&mut subscriber);
        peer.tell("publish 1");
        let got = subscriber.recv_timeout(WAIT);
        assert_eq!(got, Ok(Bytes(vec![0])), "round {round}");
        assert_eq!(peer.says(), "published");
    }
    abandon(&mut subscriber);
    let killing = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        peer.kill();
    });
    let started = Instant::now();
    assert_eq!(
        subscriber.recv_timeout(WAIT),
        Err(RecvError::Closed(Closed::PublisherDied))
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    killing.join().unwrap();
    drop(subscriber);
    runtime.shutdown_timeout(Duration::from_secs(1));
}

/// The other process's side of the tests above.
#[test]
#[ignore = "run by the tests above as their other process"]
fn child() {
    let Some(role) = std::env::var_os(ROLE) else {
        return;
    };
    init();
    let name = std::env::var(NAME).unwrap();
    let say = |line: &str| println!("topic: {line}");
    let wait_for_eof = || {
        let mut line = String::new();
        while std::io::stdin()
            .read_line(&mut line)
            .is_ok_and(|read| read != 0)
        {
            line.clear();
        }
    };
    match role.to_str().unwrap() {
        "subscribe" => {
            let topic = hub::check_service(&name).unwrap();
            let subscriber =
                Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default()).unwrap();
            say("subscribed");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line).unwrap();
            assert_eq!(line.trim(), "drop");
            drop(subscriber);
            say("dropped");
            wait_for_eof();
        }
        "subscribe-loop" => {
            let topic = hub::check_service(&name).unwrap();
            let policy = SubscriberPolicy::default();
            drop(Subscriber::<Bytes>::request(&topic, &policy).unwrap());
            say("looping");
            loop {
                drop(Subscriber::<Bytes>::request(&topic, &policy));
            }
        }
        "refuse" => {
            let topic = hub::check_service(&name).unwrap();
            let service = topic.clone().into_interface::<dyn ITopic>().unwrap();
            let token = || rsbinder::Binder::new(Inert).as_binder();
            let mut request = raw_request(token());
            request.version = 2;
            say(&format!(
                "version: {}",
                verdict(service.r#subscribe(&request))
            ));
            let other = Subscriber::<Other>::request(&topic, &SubscriberPolicy::default());
            say(&format!("type: {}", verdict(other)));
            let request = raw_request(topic.clone());
            say(&format!(
                "foreign token: {}",
                verdict(service.r#subscribe(&request))
            ));
            say("done");
            wait_for_eof();
        }
        "nofile" => {
            let topic = hub::check_service(&name).unwrap();
            // The two lowest free fds go to the control page and the parcel's duplicate of it;
            // the reply's fd would need the next.
            let lowest = File::open("/dev/null").unwrap().as_raw_fd();
            let limit = rustix::process::getrlimit(rustix::process::Resource::Nofile);
            let lowered = rustix::process::Rlimit {
                current: Some(lowest as u64 + 2),
                maximum: limit.maximum,
            };
            rustix::process::setrlimit(rustix::process::Resource::Nofile, lowered).unwrap();
            let refused = Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default());
            say(&format!("failed: {}", verdict(refused)));
            wait_for_eof();
        }
        "small-ring" => {
            let topic = hub::check_service(&name).unwrap();
            let policy = SubscriberPolicy {
                max_ring_bytes: 4096,
                ..SubscriberPolicy::default()
            };
            let refused = Subscriber::<Bytes>::request(&topic, &policy);
            say(&format!("refused: {}", verdict(refused)));
            wait_for_eof();
        }
        role @ ("publish" | "advertise") => {
            let policy = PublisherPolicy::default();
            let publisher = if role == "advertise" {
                Publisher::<Bytes>::advertise(&name, &policy).unwrap()
            } else {
                let publisher = Publisher::<Bytes>::new(&policy).unwrap();
                hub::add_service(&name, publisher.as_binder()).unwrap();
                publisher
            };
            say("ready");
            // `publish N` publishes the bytes 0 to N - 1, as many times as told, until EOF.
            for line in std::io::stdin().lines().map_while(Result::ok) {
                let count: u8 = line
                    .trim()
                    .strip_prefix("publish ")
                    .unwrap()
                    .parse()
                    .unwrap();
                for byte in 0..count {
                    publisher.publish(&Bytes(vec![byte])).unwrap();
                }
                say("published");
            }
        }
        other => panic!("no role {other}"),
    }
}

/// A refusal as one word: an `ITopic` code, or the status code.
fn verdict<T>(result: rsbinder::BinderResult<T>) -> String {
    match result {
        Ok(_) => "accepted".to_owned(),
        Err(status) if status.exception_code() == ExceptionCode::ServiceSpecific => {
            format!("specific {}", status.service_specific_error())
        }
        Err(status) => format!("{:?}", status.transaction_error()),
    }
}

/// A request as `Subscriber::request` makes it for `Bytes`, with `token` given.
fn raw_request(token: SIBinder) -> SubscribeRequest {
    SubscribeRequest {
        version: 1,
        r#type: Bytes::type_name().to_owned(),
        token: Some(token),
        control: Some(ParcelFileDescriptor::from(control_page())),
        pins: 0,
        wake: 0,
    }
}

/// A control page of layout 0.1 with no pin words, as a subscriber makes it.
fn control_page() -> OwnedFd {
    let options = CreateOptions::new("rsbinder-topic-test").populate(true);
    let file = File::from(shm::create(0x80, &options).unwrap());
    file.write_at(b"RSTC", 0).unwrap();
    file.write_at(&1u32.to_ne_bytes(), 4).unwrap();
    let fd = OwnedFd::from(file);
    let seals = Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE | Seals::SEAL;
    shm::add_seals(fd.as_fd(), seals).unwrap();
    fd
}

/// A payload of another name.
#[derive(Debug)]
struct Other;

impl Payload for Other {
    fn type_name() -> &'static str {
        "other"
    }

    fn encode(&self, _: &mut Vec<u8>) -> rsbinder::Result<()> {
        Ok(())
    }

    fn decode(_: &[u8]) -> rsbinder::Result<Self> {
        Ok(Self)
    }
}

/// A binder of the subscriber's process that answers nothing.
struct Inert;

impl rsbinder::Remotable for Inert {
    fn descriptor() -> &'static str {
        "rsbinder.topic.test.Inert"
    }

    fn on_transact(
        &self,
        _: rsbinder::TransactionCode,
        _: &mut rsbinder::Parcel,
        _: &mut rsbinder::Parcel,
    ) -> rsbinder::Result<()> {
        Err(StatusCode::UnknownTransaction)
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> rsbinder::Result<()> {
        Ok(())
    }
}

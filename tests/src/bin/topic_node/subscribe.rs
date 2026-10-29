// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `--sub` role, alone on a thread or in a `group` that one thread waits on with
//! `wait_any`, and `--fuse`, which is such a group of two subscriptions that publishes.
//!
//! Every receive goes through a [`Sub`]: the judge's checks, the `CONNECTED`, `COUNT` and
//! `ENDED` lines, and a plain subscriber's own reconnection, which asks the service manager for
//! the topic again after its end. A subscription the publisher evicted is not made again
//! (plan 15 fleet test §3.2): the publisher would take it, and evict it again.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::BinderResult;
use rsbinder_topic::{
    wait_any, Bytes, Closed, Interrupter, NamedSubscriber, Publisher, PublisherPolicy, RecvError,
    Subscriber, SubscriberPolicy, SubscriberStats, Trust, Trusted, Untrusted, Wait, Wake,
};
use tests::topic_fleet::event::{closed_reason, disconnected_reason, Kind};
use tests::topic_fleet::judge::{self, Mode, Reports, Subscription};
use tests::topic_fleet::payload::{fleet_record, FleetPayload, FleetRecord};
use tests::topic_fleet::record::{topic_id, Incarnation, Stamp};
use tests::topic_fleet::spec::{Api, PayloadKind, Recv, SubSpec, Via};
use tests::topic_fleet::transport;

use crate::node::{serve, Out, Request, Running};
use crate::publish::pub_stats;

const COUNT_EVERY: Duration = Duration::from_secs(1);
/// Between tries of a plain subscriber to subscribe again, and the longest a group's
/// `wait_any` sleeps, so that those tries go on.
const RETRY: Duration = Duration::from_millis(100);

/// A value received: its stamp, or what is wrong with it, and a T5 record's report.
struct Got {
    stamp: Result<Stamp, String>,
    report: Option<(String, Incarnation, u64)>,
}

fn got<T: FleetPayload>(value: &T, id: u32) -> Got {
    Got {
        stamp: value.stamp(id),
        report: value.report(),
    }
}

/// One subscription of any kind, as a [`Sub`] drives it.
trait Source: Send {
    /// One receive that waits, by `how`.
    fn receive(&mut self, how: Recv) -> Result<Got, RecvError>;
    /// One receive that does not wait, by `how`, for a group.
    fn try_receive(&mut self, how: Recv) -> Result<Got, RecvError>;
    fn interrupter(&self) -> Interrupter;
    fn stats(&self) -> SubscriberStats;
    fn connected(&self) -> bool;
    fn wait(&mut self) -> &mut dyn Wait;
    /// Subscribe again after the topic ended: a plain subscriber's own reconnection.
    fn resubscribe(&mut self) -> BinderResult<()>;
}

fn runtime(spec: &SubSpec) -> Option<tokio::runtime::Runtime> {
    (spec.recv == Recv::Async).then(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a tokio runtime")
    })
}

struct Plain<T: FleetPayload> {
    subscriber: Subscriber<T>,
    runtime: Option<tokio::runtime::Runtime>,
    name: String,
    policy: SubscriberPolicy,
    id: u32,
}

impl<T: FleetPayload> Source for Plain<T> {
    fn receive(&mut self, how: Recv) -> Result<Got, RecvError> {
        let value = match (how, &self.runtime) {
            (Recv::Latest, _) => self.subscriber.recv_latest(),
            (Recv::Async, Some(runtime)) => runtime.block_on(self.subscriber.recv_async()),
            _ => self.subscriber.recv(),
        }?;
        Ok(got(&value, self.id))
    }

    fn try_receive(&mut self, how: Recv) -> Result<Got, RecvError> {
        let value = match how {
            Recv::Latest => self.subscriber.try_latest(),
            _ => self.subscriber.try_recv(),
        }?;
        Ok(got(&value, self.id))
    }

    fn interrupter(&self) -> Interrupter {
        self.subscriber.interrupter()
    }

    fn stats(&self) -> SubscriberStats {
        self.subscriber.stats()
    }

    fn connected(&self) -> bool {
        true
    }

    fn wait(&mut self) -> &mut dyn Wait {
        &mut self.subscriber
    }

    fn resubscribe(&mut self) -> BinderResult<()> {
        self.subscriber = transport::subscribe(&self.name, &self.policy)?;
        Ok(())
    }
}

struct Named<T: FleetPayload> {
    subscriber: NamedSubscriber<T>,
    runtime: Option<tokio::runtime::Runtime>,
    id: u32,
}

impl<T: FleetPayload> Source for Named<T> {
    fn receive(&mut self, how: Recv) -> Result<Got, RecvError> {
        let value = match (how, &self.runtime) {
            (Recv::Latest, _) => self.subscriber.recv_latest(),
            (Recv::Async, Some(runtime)) => runtime.block_on(self.subscriber.recv_async()),
            _ => self.subscriber.recv(),
        }?;
        Ok(got(&value, self.id))
    }

    fn try_receive(&mut self, how: Recv) -> Result<Got, RecvError> {
        let value = match how {
            Recv::Latest => self.subscriber.try_latest(),
            _ => self.subscriber.try_recv(),
        }?;
        Ok(got(&value, self.id))
    }

    fn interrupter(&self) -> Interrupter {
        self.subscriber.interrupter()
    }

    fn stats(&self) -> SubscriberStats {
        self.subscriber.stats()
    }

    fn connected(&self) -> bool {
        self.subscriber.wait_connected(Duration::ZERO)
    }

    fn wait(&mut self) -> &mut dyn Wait {
        &mut self.subscriber
    }

    fn resubscribe(&mut self) -> BinderResult<()> {
        Ok(())
    }
}

/// How a `recv_sample` subscriber of either trust is made.
trait SampleTrust: Trust + Sized + Send + 'static {
    fn trust(subscriber: Subscriber<Bytes>) -> Subscriber<Bytes, Self>;
}

impl SampleTrust for Untrusted {
    fn trust(subscriber: Subscriber<Bytes>) -> Subscriber<Bytes, Self> {
        subscriber
    }
}

impl SampleTrust for Trusted {
    fn trust(subscriber: Subscriber<Bytes>) -> Subscriber<Bytes, Self> {
        // SAFETY: the publisher is a fleet node: this crate's `topic_node`, an rsbinder-topic
        // `Publisher` that follows the protocol.
        unsafe { subscriber.trust_publisher() }
    }
}

/// A `recv_sample` subscriber; a value is copied out of the sample it is lent in and checked.
struct Samples<M: SampleTrust> {
    subscriber: Subscriber<Bytes, M>,
    name: String,
    policy: SubscriberPolicy,
    id: u32,
}

impl<M: SampleTrust> Samples<M> {
    fn new(name: &str, policy: SubscriberPolicy) -> BinderResult<Self> {
        Ok(Self {
            subscriber: M::trust(transport::subscribe(name, &policy)?),
            name: name.into(),
            policy,
            id: topic_id(name),
        })
    }

    fn checked(&self, bytes: Vec<u8>) -> Got {
        got(&Bytes(bytes), self.id)
    }
}

impl<M: SampleTrust> Source for Samples<M> {
    fn receive(&mut self, _how: Recv) -> Result<Got, RecvError> {
        let bytes = self.subscriber.recv_sample()?.to_owned();
        Ok(self.checked(bytes))
    }

    fn try_receive(&mut self, _how: Recv) -> Result<Got, RecvError> {
        let bytes = self.subscriber.try_recv_sample()?.to_owned();
        Ok(self.checked(bytes))
    }

    fn interrupter(&self) -> Interrupter {
        self.subscriber.interrupter()
    }

    fn stats(&self) -> SubscriberStats {
        self.subscriber.stats()
    }

    fn connected(&self) -> bool {
        true
    }

    fn wait(&mut self) -> &mut dyn Wait {
        &mut self.subscriber
    }

    fn resubscribe(&mut self) -> BinderResult<()> {
        self.subscriber = M::trust(transport::subscribe(&self.name, &self.policy)?);
        Ok(())
    }
}

fn typed<T: FleetPayload>(
    spec: &SubSpec,
    policy: SubscriberPolicy,
) -> BinderResult<Box<dyn Source>> {
    let id = topic_id(&spec.topic);
    let runtime = runtime(spec);
    Ok(match spec.api {
        Api::Plain => Box::new(Plain::<T> {
            subscriber: transport::subscribe(&spec.topic, &policy)?,
            runtime,
            name: spec.topic.clone(),
            policy,
            id,
        }),
        Api::Named => Box::new(Named::<T> {
            subscriber: transport::subscribe_named(&spec.topic, &policy, &spec.via)?,
            runtime,
            id,
        }),
    })
}

fn source(spec: &SubSpec) -> Result<Box<dyn Source>, String> {
    let policy = SubscriberPolicy {
        start: spec.start,
        wake: spec.wake,
        ..SubscriberPolicy::default()
    };
    let made = match (spec.payload, spec.recv, spec.trust) {
        (PayloadKind::Bytes, Recv::Sample, false) => {
            Samples::<Untrusted>::new(&spec.topic, policy).map(|s| Box::new(s) as Box<dyn Source>)
        }
        (PayloadKind::Bytes, Recv::Sample, true) => {
            Samples::<Trusted>::new(&spec.topic, policy).map(|s| Box::new(s) as Box<dyn Source>)
        }
        (PayloadKind::Bytes, _, _) => typed::<Bytes>(spec, policy),
        (PayloadKind::Record, _, _) => typed::<FleetRecord>(spec, policy),
    };
    made.map_err(|status| format!("subscribe {}: {status}", spec.topic))
}

/// What a receive leads to.
#[derive(PartialEq)]
enum Flow {
    /// Receive again.
    Next,
    /// No record unread: a group goes on to its next subscription.
    Empty,
    /// The wait was interrupted: look at the requests.
    Look,
    /// The topic ended for a plain subscriber: subscribe again.
    Resubscribe,
    /// Nothing more to receive: evicted, or a failure reported.
    Idle,
}

/// What a record a subscription received makes its role do besides checking it (`--fuse`).
type OnRecord = Box<dyn FnMut(&Stamp) + Send>;

/// One subscription, its checks and its lines.
struct Sub {
    spec: SubSpec,
    source: Box<dyn Source>,
    /// The interrupter of the current subscription, which `Running::wake` uses.
    interrupter: Arc<Mutex<Interrupter>>,
    judge: Subscription,
    counted: Instant,
    reports: Reports,
    seen: HashMap<(String, Incarnation), u64>,
    resubscribe: bool,
    idle: bool,
    on_record: Option<OnRecord>,
}

impl Sub {
    fn new(spec: SubSpec) -> Result<Self, String> {
        let source = source(&spec)?;
        let mode = match spec.recv {
            Recv::Latest => Mode::Latest,
            _ => Mode::Counted,
        };
        Ok(Self {
            interrupter: Arc::new(Mutex::new(source.interrupter())),
            spec,
            source,
            judge: Subscription::new(mode),
            counted: Instant::now(),
            reports: Reports::default(),
            seen: HashMap::new(),
            resubscribe: false,
            idle: false,
            on_record: None,
        })
    }

    fn topic(&self) -> String {
        self.spec.topic.clone()
    }

    fn violation(&self, out: &Out, text: String) {
        out.send(Kind::Violation {
            topic: self.topic(),
            text,
        });
    }

    fn print(&mut self, out: &Out, count: Option<judge::Count>) {
        self.counted = Instant::now();
        if let Some(count) = count {
            out.send(Kind::Count {
                topic: self.topic(),
                received: count.received,
                lagged: count.lagged,
                first_seq: count.first_seq,
                last_seq: count.last_seq,
            });
        }
    }

    /// The connection's last `COUNT`, then `ENDED`.
    fn end(&mut self, out: &Out, reason: String) {
        let count = self.judge.end();
        self.print(out, count);
        out.send(Kind::Ended {
            topic: self.topic(),
            reason,
        });
    }

    fn record(&mut self, out: &Out, got: Got) {
        let checked = got
            .stamp
            .and_then(|stamp| Ok((stamp, self.judge.record(stamp)?)));
        let stamp = match checked {
            Ok((stamp, first)) => {
                if first {
                    out.send(Kind::Connected {
                        topic: self.topic(),
                        incarnation: stamp.incarnation,
                        first_seq: stamp.seq,
                    });
                }
                stamp
            }
            Err(text) => return self.violation(out, text),
        };
        if let Some((reporter, incarnation, seq)) = got.report {
            match self.reports.report(&reporter, incarnation, seq) {
                Ok(()) => {
                    self.seen.insert((reporter, incarnation), seq);
                }
                Err(text) => self.violation(out, text),
            }
        }
        if let Some(on_record) = &mut self.on_record {
            on_record(&stamp);
        }
    }

    fn handle(&mut self, out: &Out, received: Result<Got, RecvError>) -> Flow {
        let flow = match received {
            Ok(got) => {
                self.record(out, got);
                Flow::Next
            }
            Err(RecvError::Lagged(records)) => {
                if let Err(text) = self.judge.lagged(records) {
                    self.violation(out, text);
                }
                Flow::Next
            }
            Err(RecvError::Disconnected(why)) => {
                self.end(out, disconnected_reason(&why));
                Flow::Next
            }
            Err(RecvError::Closed(closed)) => {
                self.end(out, closed_reason(&closed).into());
                if closed == Closed::Evicted || self.spec.api == Api::Named {
                    Flow::Idle
                } else {
                    Flow::Resubscribe
                }
            }
            Err(RecvError::Decode(code)) => {
                self.violation(out, format!("a record failed to decode: {code}"));
                Flow::Next
            }
            Err(RecvError::Cancelled) => Flow::Look,
            Err(RecvError::Empty | RecvError::Timeout) => Flow::Empty,
            Err(err) => {
                out.fault(format!("recv {}: {err}", self.spec.topic));
                Flow::Idle
            }
        };
        match flow {
            Flow::Resubscribe => self.resubscribe = true,
            Flow::Idle => self.idle = true,
            _ => {}
        }
        if self.counted.elapsed() >= COUNT_EVERY {
            let count = self.judge.count();
            self.print(out, count);
        }
        flow
    }

    /// One try to subscribe again; the current interrupter is in place before it returns.
    fn try_resubscribe(&mut self) {
        if self.source.resubscribe().is_ok() {
            self.resubscribe = false;
            *self
                .interrupter
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = self.source.interrupter();
        }
    }

    fn stats(&self) -> Vec<Kind> {
        let stats = self.source.stats();
        let (node_received, node_lagged) = self.judge.totals();
        let mut kinds = vec![Kind::SubStats {
            topic: self.topic(),
            received: stats.received,
            lost: stats.lost,
            decode_errors: stats.decode_errors,
            connections: stats.connections,
            node_received,
            node_lagged,
            connected: self.source.connected(),
        }];
        kinds.extend(
            self.seen
                .iter()
                .map(|((reporter, incarnation), &seq)| Kind::ReportSeen {
                    topic: self.topic(),
                    reporter: reporter.clone(),
                    incarnation: *incarnation,
                    seq,
                }),
        );
        kinds
    }

    fn stop(&mut self, out: &Out) {
        let count = self.judge.end();
        self.print(out, count);
    }
}

/// What a group's thread does at `stats` and `exit` besides its subscriptions (`--fuse`).
struct Extra {
    stats: Box<dyn Fn() -> Vec<Kind> + Send>,
    finish: Box<dyn FnOnce() + Send>,
}

fn wake_all(interrupters: Vec<Arc<Mutex<Interrupter>>>) -> Box<dyn Fn() + Send> {
    Box::new(move || {
        for interrupter in &interrupters {
            interrupter
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .interrupt();
        }
    })
}

/// One subscription on its own thread.
fn run_one(out: Out, mut sub: Sub, requested: mpsc::Receiver<Request>) {
    loop {
        if sub.idle || sub.resubscribe {
            let wait = if sub.idle {
                Duration::from_secs(3600)
            } else {
                RETRY
            };
            match requested.recv_timeout(wait) {
                Ok(Request::Stats(answer)) => {
                    let _ = answer.send(sub.stats());
                }
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            if sub.resubscribe {
                sub.try_resubscribe();
                // A `Stop` sent before the new interrupter was in place interrupted the old one.
                if serve(&requested, || sub.stats()) {
                    break;
                }
            }
            continue;
        }
        let received = sub.source.receive(sub.spec.recv);
        if sub.handle(&out, received) == Flow::Look && serve(&requested, || sub.stats()) {
            break;
        }
    }
    sub.stop(&out);
}

/// Subscriptions with `Wake::Fd` that one thread waits on with `wait_any`.
fn run_group(
    out: Out,
    mut subs: Vec<Sub>,
    requested: mpsc::Receiver<Request>,
    extra: Option<Extra>,
) {
    let stats = |subs: &[Sub], extra: &Option<Extra>| -> Vec<Kind> {
        let mut kinds: Vec<Kind> = subs.iter().flat_map(Sub::stats).collect();
        if let Some(extra) = extra {
            kinds.extend((extra.stats)());
        }
        kinds
    };
    loop {
        for sub in subs.iter_mut().filter(|sub| sub.resubscribe) {
            sub.try_resubscribe();
        }
        // After the resubscriptions: a `Stop` sent meanwhile interrupted an old subscription.
        if serve(&requested, || stats(&subs, &extra)) {
            break;
        }
        let mut waits: Vec<&mut dyn Wait> = subs
            .iter_mut()
            .filter(|sub| !sub.idle && !sub.resubscribe)
            .map(|sub| sub.source.wait())
            .collect();
        if waits.is_empty() {
            drop(waits);
            match requested.recv_timeout(RETRY) {
                Ok(Request::Stats(answer)) => {
                    let _ = answer.send(stats(&subs, &extra));
                }
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            continue;
        }
        if let Err(err) = wait_any(&mut waits, Some(RETRY)) {
            drop(waits);
            out.fault(format!("wait_any: {err}"));
            subs.iter_mut().for_each(|sub| sub.idle = true);
            continue;
        }
        drop(waits);
        for sub in subs.iter_mut().filter(|sub| !sub.idle && !sub.resubscribe) {
            loop {
                let received = sub.source.try_receive(sub.spec.recv);
                match sub.handle(&out, received) {
                    Flow::Next => {}
                    Flow::Empty | Flow::Look | Flow::Resubscribe | Flow::Idle => break,
                }
            }
        }
    }
    for sub in &mut subs {
        sub.stop(&out);
    }
    if let Some(extra) = extra {
        (extra.finish)();
    }
}

fn spawn(out: &Out, subs: Vec<Sub>, extra: Option<Extra>, group: bool) -> Running {
    let (requests, requested) = mpsc::channel();
    let wake = wake_all(subs.iter().map(|sub| sub.interrupter.clone()).collect());
    let out = out.clone();
    let thread = if group {
        thread::spawn(move || run_group(out, subs, requested, extra))
    } else {
        let sub = subs.into_iter().next().expect("one subscription");
        thread::spawn(move || run_one(out, sub, requested))
    };
    Running {
        requests,
        wake,
        threads: vec![thread],
    }
}

/// One subscription on its own thread, or a group of them on one.
pub fn start(out: &Out, specs: Vec<SubSpec>) -> Result<Running, String> {
    let group = specs.iter().any(|spec| spec.group.is_some());
    let subs = specs
        .into_iter()
        .map(Sub::new)
        .collect::<Result<Vec<_>, _>>()?;
    Ok(spawn(out, subs, None, group))
}

fn fuse_input(topic: String, payload: PayloadKind) -> SubSpec {
    SubSpec {
        topic,
        payload,
        api: Api::Plain,
        recv: Recv::Recv,
        wake: Wake::Fd,
        start: rsbinder_topic::Start::Latest,
        trust: false,
        group: Some("fuse".into()),
        via: Via::ServiceManager,
    }
}

/// T4: a record on `topic` for each frame (T1), carrying its seq and the latest reading's (T2).
pub fn fuse(
    out: &Out,
    topic: String,
    frames: String,
    readings: String,
    incarnation: Incarnation,
) -> Result<Running, String> {
    let publisher: Arc<Publisher<FleetRecord>> = Arc::new(
        transport::advertise(&topic, &PublisherPolicy::default(), &Via::ServiceManager)
            .map_err(|status| format!("advertise {topic}: {status}"))?,
    );
    let mut frames = Sub::new(fuse_input(frames, PayloadKind::Bytes))?;
    let mut readings = Sub::new(fuse_input(readings, PayloadKind::Record))?;
    let next = Arc::new(AtomicU64::new(0));
    let reading = Arc::new(AtomicU64::new(u64::MAX));
    readings.on_record = Some(Box::new({
        let reading = reading.clone();
        move |stamp| reading.store(stamp.seq, Ordering::Relaxed)
    }));
    frames.on_record = Some(Box::new({
        let (publisher, next, out, id) = (
            publisher.clone(),
            next.clone(),
            out.clone(),
            topic_id(&topic),
        );
        move |frame| {
            let seq = next.fetch_add(1, Ordering::Relaxed);
            let stamp = Stamp {
                topic: id,
                incarnation,
                seq,
            };
            let values = vec![frame.seq as i64, reading.load(Ordering::Relaxed) as i64];
            if let Err(err) = publisher.publish(&fleet_record(&stamp, values, None)) {
                out.fault(format!("publish fused seq {seq}: {err}"));
            }
        }
    }));
    let extra = Extra {
        stats: Box::new({
            let (publisher, topic) = (publisher.clone(), topic.clone());
            move || vec![pub_stats(&topic, publisher.stats())]
        }),
        finish: Box::new({
            let out = out.clone();
            move || {
                publisher.close(&rsbinder::Status::from(rsbinder::ExceptionCode::None));
                if let Some(last_seq) = next.load(Ordering::Relaxed).checked_sub(1) {
                    out.send(Kind::Published {
                        topic,
                        incarnation,
                        last_seq,
                    });
                }
            }
        }),
    };
    Ok(spawn(out, vec![frames, readings], Some(extra), true))
}

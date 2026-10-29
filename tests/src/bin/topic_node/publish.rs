// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `--pub` role: a record `hz` times a second, through the ring or lent from the pool, and
//! the commands `close`, `readvertise` and `resume`.

use std::num::NonZeroUsize;
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::{hub, ExceptionCode, Status};
use rsbinder_topic::{Backpressure, Bytes, PoolPolicy, Publisher, PublisherPolicy, PublisherStats};
use tests::topic_fleet::event::{Kind, Subscribed};
use tests::topic_fleet::payload::{FleetPayload, FleetRecord};
use tests::topic_fleet::record::{topic_id, Incarnation, Stamp};
use tests::topic_fleet::spec::{PayloadKind, PubSpec, Via};
use tests::topic_fleet::transport;

use crate::node::{Command, Out, Request, Running};

pub fn policy(spec: &PubSpec) -> PublisherPolicy {
    let mut policy = PublisherPolicy::default();
    if let Some(ring_bytes) = spec.ring_bytes {
        policy.ring_bytes = ring_bytes;
    }
    if let Some(max) = spec.max_subscribers {
        policy.max_subscribers = max;
        policy.max_per_uid = max;
    }
    if spec.pool {
        policy.pool = Some(PoolPolicy {
            slot_bytes: spec.size,
            ..PoolPolicy::default()
        });
    }
    if let Some(ms) = spec.backpressure_ms {
        policy.backpressure = Some(Backpressure {
            timeout: Duration::from_millis(ms),
            window: spec.window.and_then(NonZeroUsize::new),
        });
    }
    policy
}

/// A `PSTATS` of `stats`.
pub fn pub_stats(topic: &str, stats: PublisherStats) -> Kind {
    Kind::PubStats {
        topic: topic.into(),
        published: stats.published,
        evicted: stats.evicted,
        subscribers: stats
            .subscribers
            .iter()
            .map(|info| Subscribed {
                pid: info.pid,
                lost: info.lost,
                evicted: info.evicted,
            })
            .collect(),
        bytes: stats.bytes,
    }
}

pub fn start(out: &Out, spec: PubSpec, incarnation: Incarnation) -> Result<Running, String> {
    match spec.payload {
        PayloadKind::Bytes => {
            let size = spec.size;
            let write: Write<Bytes> = if spec.pool {
                Box::new(move |publisher, stamp| {
                    let bytes = stamp.encode(size);
                    let mut slot = publisher
                        .loan_slice(bytes.0.len())
                        .map_err(|err| err.to_string())?;
                    slot.copy_from_slice(&bytes.0);
                    slot.publish().map_err(|err| err.to_string())
                })
            } else {
                Box::new(move |publisher, stamp| {
                    publisher
                        .publish(&Bytes::make(stamp, size))
                        .map_err(|err| err.to_string())
                })
            };
            run(out, spec, incarnation, write)
        }
        PayloadKind::Record => {
            let write: Write<FleetRecord> = Box::new(|publisher, stamp| {
                publisher
                    .publish(&FleetRecord::make(stamp, 0))
                    .map_err(|err| err.to_string())
            });
            run(out, spec, incarnation, write)
        }
    }
}

/// The publisher of `spec` and the seq it publishes next: advertised, or with `prepub` made,
/// given its first records and then registered.
fn make<T: FleetPayload>(
    spec: &PubSpec,
    incarnation: Incarnation,
    write: &Write<T>,
) -> Result<(Publisher<T>, u64), String> {
    let policy = policy(spec);
    let topic = &spec.topic;
    let Some(records) = spec.prepublish else {
        let publisher = transport::advertise(topic, &policy, &spec.via)
            .map_err(|status| format!("advertise {topic}: {status}"))?;
        return Ok((publisher, 0));
    };
    let publisher = Publisher::new(&policy).map_err(|err| format!("make {topic}: {err:?}"))?;
    let id = topic_id(topic);
    for seq in 0..records {
        let stamp = Stamp {
            topic: id,
            incarnation,
            seq,
        };
        write(&publisher, &stamp).map_err(|err| format!("publish {topic} seq {seq}: {err}"))?;
    }
    hub::add_service(topic, publisher.as_binder())
        .map_err(|status| format!("addService {topic}: {status}"))?;
    Ok((publisher, records))
}

/// End a publisher at `exit`: its end record, then `PUBLISHED` with the last seq it wrote.
pub fn finish<T: FleetPayload>(
    out: &Out,
    topic: &str,
    publisher: Publisher<T>,
    incarnation: Incarnation,
    next: u64,
) {
    publisher.close(&Status::from(ExceptionCode::None));
    drop(publisher);
    published(out, topic, incarnation, next);
}

/// `PUBLISHED` with the seq before `next`, once a record was.
fn published(out: &Out, topic: &str, incarnation: Incarnation, next: u64) {
    if let Some(last_seq) = next.checked_sub(1) {
        out.send(Kind::Published {
            topic: topic.into(),
            incarnation,
            last_seq,
        });
    }
}

type Write<T> = Box<dyn Fn(&Publisher<T>, &Stamp) -> Result<(), String> + Send>;

/// What a publisher's thread does next.
#[derive(PartialEq)]
enum State {
    Publishing,
    /// Made with `prepub`, until `resume`.
    Paused,
    /// Found through a registry that could not be reached: no publisher until `rebind`.
    Unbound,
    /// After `close`, or a publish that failed: it answers `stats` and `exit` only.
    Done,
}

fn run<T: FleetPayload>(
    out: &Out,
    spec: PubSpec,
    incarnation: Incarnation,
    write: Write<T>,
) -> Result<Running, String> {
    let through_registry = matches!(spec.via, Via::Registry(_));
    let (mut publisher, mut seq, mut state) = match make(&spec, incarnation, &write) {
        Ok((publisher, seq)) if spec.prepublish.is_some() => (Some(publisher), seq, State::Paused),
        Ok((publisher, seq)) => (Some(publisher), seq, State::Publishing),
        Err(text) if through_registry => {
            out.send(Kind::Unbound {
                topic: spec.topic.clone(),
                text,
            });
            (None, 0, State::Unbound)
        }
        Err(text) => return Err(text),
    };
    let (requests, requested) = mpsc::channel();
    let period = Duration::from_secs(1) / spec.hz;
    let id = topic_id(&spec.topic);
    let topic = spec.topic.clone();
    let out = out.clone();
    let thread = thread::spawn(move || {
        let mut next_at = Instant::now();
        loop {
            let wait = match state {
                State::Publishing => next_at.saturating_duration_since(Instant::now()),
                State::Paused | State::Unbound | State::Done => Duration::from_secs(3600),
            };
            match requested.recv_timeout(wait) {
                Ok(Request::Stats(answer)) => {
                    let stats = publisher.as_ref().map(Publisher::stats).unwrap_or_default();
                    let _ = answer.send(vec![pub_stats(&spec.topic, stats)]);
                    continue;
                }
                Ok(Request::Command(Command::Resume, _)) if state == State::Paused => {
                    state = State::Publishing;
                    next_at = Instant::now();
                    continue;
                }
                Ok(Request::Command(Command::Close, _)) if state == State::Publishing => {
                    if let Some(publisher) = &publisher {
                        publisher.close(&Status::from(ExceptionCode::None));
                    }
                    published(&out, &spec.topic, incarnation, seq);
                    state = State::Done;
                    continue;
                }
                Ok(Request::Command(Command::Readvertise, _)) if publisher.is_some() => {
                    let binder = publisher.as_ref().map(Publisher::as_binder);
                    if let Some(Err(status)) =
                        binder.map(|binder| hub::add_service(&spec.topic, binder))
                    {
                        out.fault(format!("addService {}: {status}", spec.topic));
                    }
                    continue;
                }
                // The old publisher ends (its end record); the new one goes on with the seqs, so
                // that a connection to it returns none an earlier one did.
                Ok(Request::Command(Command::Rebind, _))
                    if through_registry && state != State::Done =>
                {
                    if let Some(old) = publisher.take() {
                        old.close(&Status::from(ExceptionCode::None));
                    }
                    match transport::advertise(&spec.topic, &policy(&spec), &spec.via) {
                        Ok(new) => {
                            publisher = Some(new);
                            state = State::Publishing;
                            next_at = Instant::now();
                        }
                        Err(status) => {
                            out.send(Kind::Unbound {
                                topic: spec.topic.clone(),
                                text: format!("advertise: {status}"),
                            });
                            state = State::Unbound;
                        }
                    }
                    continue;
                }
                Ok(Request::Command(command, _)) => {
                    out.fault(format!(
                        "{command:?} {}: not for this publisher now",
                        spec.topic
                    ));
                    continue;
                }
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) if state != State::Publishing => continue,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let Some(current) = &publisher else { continue };
            let stamp = Stamp {
                topic: id,
                incarnation,
                seq,
            };
            if let Err(err) = write(current, &stamp) {
                out.fault(format!("publish {} seq {seq}: {err}", spec.topic));
                state = State::Done;
                continue;
            }
            seq += 1;
            next_at += period;
        }
        match publisher {
            Some(publisher) if state != State::Done => {
                finish(&out, &spec.topic, publisher, incarnation, seq)
            }
            _ => drop(publisher),
        }
    });
    Ok(Running::thread(requests, thread).for_topic(&topic))
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The `--pub` role: a record `hz` times a second, through the ring or lent from the pool.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::{ExceptionCode, Status};
use rsbinder_topic::{Backpressure, Bytes, PoolPolicy, Publisher, PublisherPolicy, PublisherStats};
use tests::topic_fleet::event::{Kind, Subscribed};
use tests::topic_fleet::payload::{FleetPayload, FleetRecord};
use tests::topic_fleet::record::{topic_id, Incarnation, Stamp};
use tests::topic_fleet::spec::{PayloadKind, PubSpec};
use tests::topic_fleet::transport;

use crate::node::{Out, Request, Running};

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
            ..Backpressure::default()
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
    }
}

pub fn start(out: &Out, spec: PubSpec, incarnation: Incarnation) -> Result<Running, String> {
    let policy = policy(&spec);
    match spec.payload {
        PayloadKind::Bytes => {
            let publisher: Publisher<Bytes> = transport::advertise(&spec.topic, &policy, &spec.via)
                .map_err(|status| format!("advertise {}: {status}", spec.topic))?;
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
            Ok(run(out, spec, publisher, incarnation, write))
        }
        PayloadKind::Record => {
            let publisher: Publisher<FleetRecord> =
                transport::advertise(&spec.topic, &policy, &spec.via)
                    .map_err(|status| format!("advertise {}: {status}", spec.topic))?;
            let write = Box::new(|publisher: &Publisher<FleetRecord>, stamp: &Stamp| {
                publisher
                    .publish(&FleetRecord::make(stamp, 0))
                    .map_err(|err| err.to_string())
            });
            Ok(run(out, spec, publisher, incarnation, write))
        }
    }
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
    if let Some(last_seq) = next.checked_sub(1) {
        out.send(Kind::Published {
            topic: topic.into(),
            incarnation,
            last_seq,
        });
    }
}

type Write<T> = Box<dyn Fn(&Publisher<T>, &Stamp) -> Result<(), String> + Send>;

fn run<T: FleetPayload>(
    out: &Out,
    spec: PubSpec,
    publisher: Publisher<T>,
    incarnation: Incarnation,
    write: Write<T>,
) -> Running {
    let (requests, requested) = mpsc::channel();
    let period = Duration::from_secs(1) / spec.hz;
    let id = topic_id(&spec.topic);
    let out = out.clone();
    let thread = thread::spawn(move || {
        let mut next_at = Instant::now();
        let mut seq = 0;
        loop {
            match requested.recv_timeout(next_at.saturating_duration_since(Instant::now())) {
                Ok(Request::Stats(answer)) => {
                    let _ = answer.send(vec![pub_stats(&spec.topic, publisher.stats())]);
                    continue;
                }
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let stamp = Stamp {
                topic: id,
                incarnation,
                seq,
            };
            if let Err(err) = write(&publisher, &stamp) {
                out.fault(format!("publish {} seq {seq}: {err}", spec.topic));
                // The thread stays to answer `stats` and `exit`.
                next_at += Duration::from_secs(3600);
                continue;
            }
            seq += 1;
            next_at += period;
        }
        finish(&out, &spec.topic, publisher, incarnation, seq);
    });
    Running::thread(requests, thread)
}

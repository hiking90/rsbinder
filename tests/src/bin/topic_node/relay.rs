// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The roles that serve a binder: `--relay` (T5, the oneway fan-in the topic design leans on
//! instead of many producers per topic, plan 15 fleet test §1.2), its `--report`ers, and
//! `--registry`, the topic registry T6 is found through.

use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use rsbinder::{hub, BinderResult, ExceptionCode, Interface, Status, Strong};
use rsbinder_topic::registry::{AllowAll, Registry};
use rsbinder_topic::{Publisher, PublisherPolicy};
use tests::topic_fleet::event::Kind;
use tests::topic_fleet::payload::{
    fleet_record, BnFleetStatus, FleetRecord, IFleetStatus, StatusReport,
};
use tests::topic_fleet::record::{topic_id, Incarnation, Stamp};
use tests::topic_fleet::spec::Via;
use tests::topic_fleet::transport;

use crate::node::{Out, Request, Running};
use crate::publish::pub_stats;

/// The relay's publisher and the seq of its next record, under one lock: records go out in the
/// order the reports were taken.
struct Relayed {
    publisher: Option<Publisher<FleetRecord>>,
    next: u64,
}

struct Relay {
    relayed: Arc<Mutex<Relayed>>,
    id: u32,
    incarnation: Incarnation,
    out: Out,
}

impl Interface for Relay {}

impl IFleetStatus for Relay {
    fn r#report(&self, report: &StatusReport) -> BinderResult<()> {
        let mut relayed = self.relayed.lock().unwrap_or_else(PoisonError::into_inner);
        let seq = relayed.next;
        let Some(publisher) = &relayed.publisher else {
            return Ok(());
        };
        let stamp = Stamp {
            topic: self.id,
            incarnation: self.incarnation,
            seq,
        };
        let report = StatusReport {
            reporter: report.reporter.clone(),
            pid: report.pid,
            start: report.start,
            seq: report.seq,
        };
        match publisher.publish(&fleet_record(&stamp, Vec::new(), Some(report))) {
            Ok(()) => relayed.next += 1,
            Err(err) => self.out.fault(format!("relay seq {seq}: {err}")),
        }
        Ok(())
    }
}

/// T5: serve `IFleetStatus` under `service` and publish each report on `topic`.
pub fn relay(
    out: &Out,
    topic: String,
    service: String,
    incarnation: Incarnation,
) -> Result<Running, String> {
    let publisher: Publisher<FleetRecord> =
        transport::advertise(&topic, &PublisherPolicy::default(), &Via::ServiceManager)
            .map_err(|status| format!("advertise {topic}: {status}"))?;
    let relayed = Arc::new(Mutex::new(Relayed {
        publisher: Some(publisher),
        next: 0,
    }));
    let relay = Relay {
        relayed: relayed.clone(),
        id: topic_id(&topic),
        incarnation,
        out: out.clone(),
    };
    hub::add_service(&service, BnFleetStatus::new_binder(relay).as_binder())
        .map_err(|status| format!("addService {service}: {status}"))?;
    let (requests, requested) = mpsc::channel();
    let out = out.clone();
    let thread = thread::spawn(move || {
        while let Ok(Request::Stats(answer)) = requested.recv() {
            let relayed = relayed.lock().unwrap_or_else(PoisonError::into_inner);
            let stats = relayed
                .publisher
                .as_ref()
                .map(Publisher::stats)
                .unwrap_or_default();
            let _ = answer.send(vec![pub_stats(&topic, stats)]);
        }
        let mut relayed = relayed.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(publisher) = relayed.publisher.take() {
            publisher.close(&Status::from(ExceptionCode::None));
        }
        if let Some(last_seq) = relayed.next.checked_sub(1) {
            out.send(Kind::Published {
                topic,
                incarnation,
                last_seq,
            });
        }
    });
    Ok(Running::thread(requests, thread))
}

/// Report to the relay under `service` `hz` times a second, finding it again after it died.
pub fn report(out: &Out, service: String, hz: u32, incarnation: Incarnation) -> Running {
    let (requests, requested) = mpsc::channel();
    let period = Duration::from_secs(1) / hz;
    let reporter = out.node().to_string();
    let thread = thread::spawn(move || {
        let mut relay: Option<Strong<dyn IFleetStatus>> = None;
        let mut sent = 0u64;
        let mut next_at = Instant::now();
        loop {
            match requested.recv_timeout(next_at.saturating_duration_since(Instant::now())) {
                Ok(Request::Stats(answer)) => {
                    let _ = answer.send(vec![Kind::Reported {
                        service: service.clone(),
                        sent,
                    }]);
                    continue;
                }
                // A reporter has no topic, so no command names it.
                Ok(Request::Command(_)) => continue,
                Ok(Request::Stop) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            next_at += period;
            if relay.is_none() {
                relay = hub::check_interface::<dyn IFleetStatus>(&service).ok();
            }
            let Some(to) = &relay else {
                continue;
            };
            let report = StatusReport {
                reporter: reporter.clone(),
                pid: incarnation.pid as i32,
                start: incarnation.start as i32,
                seq: sent as i64,
            };
            match to.r#report(&report) {
                Ok(()) => sent += 1,
                Err(_) => relay = None,
            }
        }
    });
    Running::thread(requests, thread)
}

/// Host a topic registry under `service`.
pub fn registry(service: String) -> Result<Running, String> {
    let registry = Registry::new(AllowAll);
    hub::add_service(&service, registry.as_binder())
        .map_err(|status| format!("addService {service}: {status}"))?;
    let (requests, requested) = mpsc::channel();
    let thread = thread::spawn(move || {
        while let Ok(Request::Stats(answer)) = requested.recv() {
            let stats = registry.stats();
            let _ = answer.send(vec![Kind::RegStats {
                service: service.clone(),
                topics: stats.topics,
                watchers: stats.watchers,
            }]);
        }
        drop(registry);
    });
    Ok(Running::thread(requests, thread))
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use super::{TransactionObserver, TxnContext};
use crate::{binder::TransactionCode, Result};

/// Number of buckets in a [`LatencyHistogram`].
///
/// Bucket 0 holds handler times under 1 µs, bucket `i` (1 ≤ i < 24) those in
/// `[2^(i-1), 2^i)` µs, and the last bucket everything from 2^23 µs
/// (≈ 8.4 s) up. The open bucket starts above the 5 s at which Android
/// declares an input-dispatch ANR, so a handler that slow still lands in a
/// bounded bucket of its own.
pub const LATENCY_BUCKETS: usize = 25;

/// Handler-time distribution in power-of-two microsecond buckets; see
/// [`LATENCY_BUCKETS`] for the bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencyHistogram {
    counts: [u64; LATENCY_BUCKETS],
}

impl Default for LatencyHistogram {
    fn default() -> Self {
        Self {
            counts: [0; LATENCY_BUCKETS],
        }
    }
}

impl LatencyHistogram {
    fn bucket(elapsed: Duration) -> usize {
        let micros = elapsed.as_micros();
        if micros == 0 {
            return 0;
        }
        // `micros` in [2^(i-1), 2^i) → bucket i.
        let bits = (u128::BITS - micros.leading_zeros()) as usize;
        bits.min(LATENCY_BUCKETS - 1)
    }

    fn record(&mut self, elapsed: Duration) {
        self.counts[Self::bucket(elapsed)] += 1;
    }

    /// Per-bucket counts, index `i` as described at [`LATENCY_BUCKETS`].
    pub fn counts(&self) -> &[u64; LATENCY_BUCKETS] {
        &self.counts
    }

    /// Exclusive upper bound of bucket `i`, `None` for the last (open)
    /// bucket or an index past it.
    pub fn upper_bound(i: usize) -> Option<Duration> {
        (i < LATENCY_BUCKETS - 1).then(|| Duration::from_micros(1 << i))
    }
}

/// Counters for one `(descriptor, code)` pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MethodStats {
    /// Interface descriptor of the served object.
    pub descriptor: String,
    /// Transaction code.
    pub code: TransactionCode,
    /// AIDL method name, when the interface carries a name table.
    pub method: Option<&'static str>,
    /// Transactions dispatched.
    pub calls: u64,
    /// Of those, how many the handler failed at the transport level (see
    /// [`TransactionObserver::on_reply`]'s `result`). An AIDL exception or
    /// service-specific error is written into the reply and is not counted.
    pub errors: u64,
    /// Sum of handler times.
    pub total: Duration,
    /// Longest handler time.
    pub max: Duration,
    /// Handler-time distribution.
    pub latency: LatencyHistogram,
}

/// A copy of a [`StatsObserver`]'s counters at one point in time.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct StatsSnapshot {
    /// One entry per `(descriptor, code)` seen, sorted by both.
    pub methods: Vec<MethodStats>,
    /// Transactions being served when the snapshot was taken.
    pub in_flight: usize,
    /// Most transactions served at once since the observer was created.
    pub max_in_flight: usize,
}

/// Per-method call counts, transport error counts and handler-time
/// histograms, plus how many transactions are being served at once.
///
/// `max_in_flight` reaching the binder thread-pool size (`max_threads` of
/// [`ProcessState::init`](crate::ProcessState::init) plus the threads that
/// joined the pool themselves) is necessary but not sufficient for the pool
/// having been exhausted: a nested callback on the same thread counts twice. The count covers every transport the observer sees, so an
/// RPC server's threads add to it too; filter by
/// [`TxnContext::transport`] in a wrapper to separate them.
///
/// Install a shared handle and keep one to read from:
///
/// ```no_run
/// use std::sync::Arc;
/// use rsbinder::observe::{set_observer, StatsObserver};
///
/// let stats = Arc::new(StatsObserver::new());
/// set_observer(Some(stats.clone()));
/// // ... serve ...
/// for m in stats.snapshot().methods {
///     println!("{} {:?}: {} calls", m.descriptor, m.method, m.calls);
/// }
/// ```
#[derive(Debug, Default)]
pub struct StatsObserver {
    // Descriptor first, so the per-call lookup borrows `&str` instead of allocating.
    methods: Mutex<HashMap<String, HashMap<TransactionCode, MethodStats>>>,
    in_flight: AtomicUsize,
    max_in_flight: AtomicUsize,
}

impl StatsObserver {
    /// An observer with every counter at zero.
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy the counters out.
    pub fn snapshot(&self) -> StatsSnapshot {
        let mut methods: Vec<MethodStats> = self
            .methods
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .flat_map(|codes| codes.values().cloned())
            .collect();
        methods.sort_by(|a, b| (&a.descriptor, a.code).cmp(&(&b.descriptor, b.code)));
        StatsSnapshot {
            methods,
            in_flight: self.in_flight.load(Ordering::Relaxed),
            max_in_flight: self.max_in_flight.load(Ordering::Relaxed),
        }
    }
}

impl TransactionObserver for StatsObserver {
    fn on_transact(&self, _ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        let now = self.in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_in_flight.fetch_max(now, Ordering::Relaxed);
        None
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        _tag: Option<Box<dyn Any + Send>>,
        result: &Result<()>,
        elapsed: Duration,
    ) {
        // Saturate: the trait is public, so a caller may send an unpaired reply.
        let _ = self
            .in_flight
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(1));

        let mut methods = self.methods.lock().unwrap_or_else(PoisonError::into_inner);
        let codes = match methods.get_mut(ctx.descriptor) {
            Some(codes) => codes,
            None => methods.entry(ctx.descriptor.to_owned()).or_default(),
        };
        let stats = codes.entry(ctx.code).or_insert_with(|| MethodStats {
            descriptor: ctx.descriptor.to_owned(),
            code: ctx.code,
            method: ctx.method,
            ..Default::default()
        });
        stats.calls += 1;
        stats.errors += u64::from(result.is_err());
        stats.total += elapsed;
        stats.max = stats.max.max(elapsed);
        stats.latency.record(elapsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{StatusCode, TransportCaps};

    fn ctx(descriptor: &str, code: TransactionCode) -> TxnContext<'_> {
        TxnContext {
            descriptor,
            code,
            method: Some("m"),
            is_oneway: false,
            calling_uid: 0,
            calling_pid: 0,
            transport: TransportCaps::KERNEL,
        }
    }

    #[test]
    fn histogram_buckets_are_powers_of_two_microseconds() {
        let us = Duration::from_micros;
        assert_eq!(LatencyHistogram::bucket(Duration::from_nanos(999)), 0);
        assert_eq!(LatencyHistogram::bucket(us(1)), 1);
        assert_eq!(LatencyHistogram::bucket(us(2)), 2);
        assert_eq!(LatencyHistogram::bucket(us(3)), 2);
        assert_eq!(LatencyHistogram::bucket(us(4)), 3);
        assert_eq!(LatencyHistogram::bucket(us((1 << 23) - 1)), 23);
        assert_eq!(LatencyHistogram::bucket(us(1 << 23)), 24);
        assert_eq!(LatencyHistogram::bucket(Duration::MAX), 24);
        // Every bucket's upper bound is the lower bound of the next.
        for i in 1..LATENCY_BUCKETS - 1 {
            let bound = LatencyHistogram::upper_bound(i).unwrap();
            assert_eq!(LatencyHistogram::bucket(bound - Duration::from_nanos(1)), i);
            assert_eq!(LatencyHistogram::bucket(bound), i + 1);
        }
        assert_eq!(LatencyHistogram::upper_bound(LATENCY_BUCKETS - 1), None);
    }

    #[test]
    fn counts_per_method_and_in_flight() {
        let stats = StatsObserver::new();
        let (a, b) = (ctx("x.IA", 1), ctx("x.IB", 2));

        stats.on_transact(&a);
        stats.on_transact(&b);
        assert_eq!(stats.snapshot().in_flight, 2);
        stats.on_reply(&a, None, &Ok(()), Duration::from_micros(3));
        stats.on_reply(
            &b,
            None,
            &Err(StatusCode::DeadObject),
            Duration::from_millis(1),
        );
        stats.on_transact(&a);
        stats.on_reply(&a, None, &Ok(()), Duration::from_micros(5));
        // An unpaired reply does not wrap the gauge.
        stats.on_reply(&a, None, &Ok(()), Duration::ZERO);

        let snap = stats.snapshot();
        assert_eq!((snap.in_flight, snap.max_in_flight), (0, 2));
        let [ia, ib] = &snap.methods[..] else {
            panic!("{:?}", snap.methods)
        };
        assert_eq!((ia.descriptor.as_str(), ia.code), ("x.IA", 1));
        assert_eq!((ia.calls, ia.errors), (3, 0));
        assert_eq!(ia.total, Duration::from_micros(8));
        assert_eq!(ia.max, Duration::from_micros(5));
        assert_eq!(ia.latency.counts().iter().sum::<u64>(), 3);
        assert_eq!((ib.calls, ib.errors, ib.method), (1, 1, Some("m")));
    }
}

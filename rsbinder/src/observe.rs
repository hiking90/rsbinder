// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Server-side transaction observers.
//!
//! A [`TransactionObserver`] installed with [`set_observer`] is called
//! around every incoming transaction this process serves, on kernel binder
//! and over RPC alike: [`on_transact`](TransactionObserver::on_transact)
//! before the service's handler runs and
//! [`on_reply`](TransactionObserver::on_reply) after it returns, before the
//! reply is sent. It is the place for per-method counters, latency
//! histograms, logging and tracing spans that would otherwise have to be
//! written into every handler. The closest AOSP counterpart is Java's
//! `Binder.setObserver` / `BinderCallsStats`; C++ and NDK libbinder have
//! none, so this API is rsbinder's own and has no wire effect.
//!
//! # What is observed
//!
//! Every transaction delivered to a local binder object: AIDL methods,
//! one-way calls (`on_reply` still runs once, with the handler's result),
//! and the binder meta transactions that reach the object (`PING`,
//! `INTERFACE`, `DUMP`, …) on kernel binder. Over RPC, the session answers
//! `INTERFACE_TRANSACTION` and `PING_TRANSACTION` itself before dispatch,
//! so those two are not observed there.
//!
//! [`TxnContext::method`] names the AIDL method when the interface was
//! generated with `rsbinder_aidl::Builder::trace(true)`; otherwise only the
//! transaction code identifies it.
//!
//! # Cost
//!
//! With no observer installed, a dispatch pays one atomic load. With one
//! installed, it also takes a read lock to clone the observer's `Arc`, reads
//! the clock twice, and makes the two calls — all on the dispatching
//! thread, so an observer's own cost is added to the caller's latency.
//!
//! # Rules for implementations
//!
//! - Calls are made on the thread that serves the transaction, with no
//!   rsbinder lock held and no thread-state borrow outstanding, so an
//!   observer may itself make binder calls.
//! - Every `on_transact` is followed by exactly one `on_reply` on the same
//!   thread; the value `on_transact` returns comes back as `tag`.
//! - A panic in either call is caught and logged. It does not change the
//!   transaction's result, and `on_reply` still runs after a panicking
//!   `on_transact` (with `tag = None`).
//! - `result` is the transport-level result of the handler. An AIDL method
//!   that returns an exception or service-specific error writes it into the
//!   reply and still reports `Ok(())` here.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::{binder::TransactionCode, Result, TransportCaps};

/// What an observer learns about one incoming transaction.
///
/// `#[non_exhaustive]`: fields may be added in minor releases.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TxnContext<'a> {
    /// Interface descriptor of the local object the transaction targets.
    pub descriptor: &'a str,
    /// Transaction code.
    pub code: TransactionCode,
    /// AIDL method name for `code`, when the interface carries a name table
    /// (see [`Remotable::transaction_name`](crate::Remotable::transaction_name)).
    pub method: Option<&'static str>,
    /// Whether the caller expects no reply.
    pub is_oneway: bool,
    /// Caller's uid as [`get_calling_uid`](crate::get_calling_uid) reports
    /// it inside the handler, sampled before the handler runs.
    pub calling_uid: crate::sys::binder::uid_t,
    /// Caller's pid as [`get_calling_pid`](crate::get_calling_pid) reports
    /// it inside the handler, sampled before the handler runs. `0` for a
    /// one-way kernel binder transaction: the driver reports no sender pid
    /// when no sending thread waits for a reply (`binder.c`
    /// `binder_thread_read`). RPC reports the connection's peer pid either way.
    pub calling_pid: crate::sys::binder::pid_t,
    /// The transport the transaction arrived over:
    /// [`TransportCaps::KERNEL`] for kernel binder, the session's
    /// capabilities for RPC.
    pub transport: TransportCaps,
}

/// Called around every incoming transaction; see the [module
/// docs](self) for when, on which thread, and what is guaranteed.
pub trait TransactionObserver: Send + Sync {
    /// Called before the handler runs. The returned value is handed back to
    /// [`on_reply`](Self::on_reply) for the same transaction — a start
    /// marker, a span guard, anything per-call.
    fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>>;

    /// Called after the handler returns and before the reply is sent.
    /// `elapsed` is the time spent in the handler, `on_transact` excluded.
    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        tag: Option<Box<dyn Any + Send>>,
        result: &Result<()>,
        elapsed: Duration,
    );
}

static INSTALLED: AtomicBool = AtomicBool::new(false);
static OBSERVER: RwLock<Option<Arc<dyn TransactionObserver>>> = RwLock::new(None);

/// Install the process-wide transaction observer, replacing any previous
/// one; `None` removes it. Transactions already being served finish with
/// the observer they started with.
///
/// There is one observer per process because kernel binder serves every
/// object from one process-wide thread pool; an observer that only cares
/// about some transactions filters on [`TxnContext::descriptor`] or
/// [`TxnContext::transport`]. To run several, install one that forwards to
/// each.
pub fn set_observer(observer: Option<Arc<dyn TransactionObserver>>) {
    let mut slot = OBSERVER.write().unwrap_or_else(PoisonError::into_inner);
    INSTALLED.store(observer.is_some(), Ordering::Release);
    *slot = observer;
}

fn current() -> Option<Arc<dyn TransactionObserver>> {
    if !INSTALLED.load(Ordering::Acquire) {
        return None;
    }
    OBSERVER
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone()
}

fn log_panic(which: &str, payload: &(dyn Any + Send)) {
    let msg = payload
        .downcast_ref::<&'static str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("<non-string panic payload>");
    log::error!("TransactionObserver::{which} panicked: {msg}");
}

/// Run `dispatch` between the observer's two calls. `ctx` is only built
/// when an observer is installed, so its lookups cost nothing otherwise.
pub(crate) fn observed<'a>(
    ctx: impl FnOnce() -> TxnContext<'a>,
    dispatch: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let Some(observer) = current() else {
        return dispatch();
    };
    let ctx = ctx();
    let tag = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.on_transact(&ctx)))
        .unwrap_or_else(|payload| {
            log_panic("on_transact", &*payload);
            None
        });
    let start = Instant::now();
    let result = dispatch();
    let elapsed = start.elapsed();
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observer.on_reply(&ctx, tag, &result, elapsed)
    })) {
        log_panic("on_reply", &*payload);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusCode;
    use std::sync::Mutex;

    fn ctx() -> TxnContext<'static> {
        TxnContext {
            descriptor: "x.y.IZ",
            code: 7,
            method: Some("m"),
            is_oneway: false,
            calling_uid: 1,
            calling_pid: 2,
            transport: TransportCaps::KERNEL,
        }
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
        panic_in: Option<&'static str>,
    }

    // Other lib tests dispatch real transactions in parallel while this one's
    // observer is installed; only the context built here is recorded.
    impl TransactionObserver for Recorder {
        fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
            if ctx.descriptor != "x.y.IZ" {
                return None;
            }
            self.events
                .lock()
                .unwrap()
                .push(format!("transact {}", ctx.code));
            if self.panic_in == Some("on_transact") {
                panic!("observer bug");
            }
            Some(Box::new(42u32))
        }

        fn on_reply(
            &self,
            ctx: &TxnContext<'_>,
            tag: Option<Box<dyn Any + Send>>,
            result: &Result<()>,
            _elapsed: Duration,
        ) {
            if ctx.descriptor != "x.y.IZ" {
                return;
            }
            let tag = tag.and_then(|t| t.downcast::<u32>().ok()).map(|t| *t);
            self.events
                .lock()
                .unwrap()
                .push(format!("reply {} {tag:?} {result:?}", ctx.code));
            if self.panic_in == Some("on_reply") {
                panic!("observer bug");
            }
        }
    }

    // One test: the observer is process-wide state.
    #[test]
    #[serial_test::serial(observer)]
    fn observer_pairs_calls_passes_the_tag_and_survives_its_own_panics() {
        let dispatches = Mutex::new(0);
        let run = |result: Result<()>| {
            observed(ctx, || {
                *dispatches.lock().unwrap() += 1;
                result
            })
        };

        // Nothing installed: the handler still runs and the context is not built.
        assert_eq!(
            observed(
                || unreachable!("context built without an observer"),
                || Ok(())
            ),
            Ok(())
        );

        let recorder = Arc::new(Recorder::default());
        set_observer(Some(recorder.clone()));
        assert_eq!(run(Err(StatusCode::BadValue)), Err(StatusCode::BadValue));
        assert_eq!(
            *recorder.events.lock().unwrap(),
            ["transact 7", "reply 7 Some(42) Err(BadValue)"]
        );

        for which in ["on_transact", "on_reply"] {
            let panicking = Arc::new(Recorder {
                panic_in: Some(which),
                ..Default::default()
            });
            set_observer(Some(panicking.clone()));
            assert_eq!(run(Ok(())), Ok(()), "a panic in {which} changed the result");
            let events = panicking.events.lock().unwrap();
            assert_eq!(events.len(), 2, "{which}: {events:?}");
            let expected_tag = if which == "on_transact" {
                "None"
            } else {
                "Some(42)"
            };
            assert_eq!(events[1], format!("reply 7 {expected_tag} Ok(())"));
        }

        set_observer(None);
        assert_eq!(run(Ok(())), Ok(()));
        assert_eq!(*dispatches.lock().unwrap(), 4);
    }
}

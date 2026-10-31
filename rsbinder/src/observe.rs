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
//!   `on_transact` (with `tag = None`). The same holds for a panic in the
//!   user [`Remotable::transaction_name`](crate::Remotable::transaction_name)
//!   impl that builds the [`TxnContext`], and on kernel binder for
//!   `Remotable::descriptor`: it is logged, and
//!   the transaction is dispatched without either observer call. On RPC
//!   the dispatch calls `descriptor` again outside that guard, so a
//!   panicking `descriptor` still unwinds the serve loop there.
//! - `result` is the transport-level result of the handler. An AIDL method
//!   that returns an exception or service-specific error writes it into the
//!   reply and still reports `Ok(())` here.
//!
//! # Provided observers
//!
//! - [`LogObserver`] — one `debug` log line per transaction.
//! - [`StatsObserver`] — per-method call and error counts, handler-time
//!   histograms, and the number of transactions served at once.
//! - `TracingObserver` (feature `tracing`) — a `tracing` span per
//!   transaction, named `AIDL::rust::<descriptor>::<method>::server` as AOSP
//!   names its ATrace sections. Proxies generated with
//!   `rsbinder_aidl::Builder::trace(true)` open the matching `::client`
//!   span under the same feature.
//!
//! Only one observer is installed at a time; to combine these, install a
//! [`Fanout`] of them. [`observer`] returns the one installed, and
//! [`replace_observer`] installs one and returns the one it replaced.

use std::any::Any;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::{binder::TransactionCode, Result, TransportCaps};

mod log_observer;
mod span;
mod stats;

pub use log_observer::LogObserver;
#[cfg(feature = "tracing")]
pub use span::TracingObserver;
#[doc(hidden)]
pub use span::{__ClientSpan, __trace_client};
pub use stats::{LatencyHistogram, MethodStats, StatsObserver, StatsSnapshot, LATENCY_BUCKETS};

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

impl<'a> TxnContext<'a> {
    /// A context for a two-way kernel binder transaction with `code` on
    /// `descriptor`: no method name, caller uid and pid `0`, transport
    /// [`TransportCaps::KERNEL`]. The `with_*` methods set the other fields.
    ///
    /// The dispatcher builds the contexts an installed observer receives;
    /// this constructor exists so an observer can be unit-tested without a
    /// binder transaction.
    pub const fn new(descriptor: &'a str, code: TransactionCode) -> Self {
        Self {
            descriptor,
            code,
            method: None,
            is_oneway: false,
            calling_uid: 0,
            calling_pid: 0,
            transport: TransportCaps::KERNEL,
        }
    }

    /// Set [`method`](Self::method).
    pub const fn with_method(mut self, method: &'static str) -> Self {
        self.method = Some(method);
        self
    }

    /// Set [`is_oneway`](Self::is_oneway).
    pub const fn with_oneway(mut self, is_oneway: bool) -> Self {
        self.is_oneway = is_oneway;
        self
    }

    /// Set [`calling_uid`](Self::calling_uid).
    pub const fn with_calling_uid(mut self, uid: crate::sys::binder::uid_t) -> Self {
        self.calling_uid = uid;
        self
    }

    /// Set [`calling_pid`](Self::calling_pid).
    pub const fn with_calling_pid(mut self, pid: crate::sys::binder::pid_t) -> Self {
        self.calling_pid = pid;
        self
    }

    /// Set [`transport`](Self::transport).
    pub const fn with_transport(mut self, transport: TransportCaps) -> Self {
        self.transport = transport;
        self
    }
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
/// [`TxnContext::transport`]. To run several, install a [`Fanout`].
///
/// The previous observer is dropped here, after the slot's lock is
/// released. Code that installs an observer into a process it does not own
/// (a library) should use [`replace_observer`] and keep what it replaced,
/// or read [`observer`] first and install a [`Fanout`] of both.
pub fn set_observer(observer: Option<Arc<dyn TransactionObserver>>) {
    drop(replace_observer(observer));
}

/// Install the process-wide transaction observer like [`set_observer`]
/// and return the one it replaced, `None` if there was none.
///
/// The returned observer may still be serving transactions that started
/// before the call; they finish with it.
pub fn replace_observer(
    observer: Option<Arc<dyn TransactionObserver>>,
) -> Option<Arc<dyn TransactionObserver>> {
    let mut slot = OBSERVER.write().unwrap_or_else(PoisonError::into_inner);
    INSTALLED.store(observer.is_some(), Ordering::Release);
    // Returned after the guard drops, so the old observer's user `Drop` never runs under the lock.
    std::mem::replace(&mut *slot, observer)
}

/// The process-wide transaction observer installed now, `None` if there
/// is none.
pub fn observer() -> Option<Arc<dyn TransactionObserver>> {
    current()
}

/// An observer that forwards every call to each of several observers, in
/// order.
///
/// Each observer gets its own `tag` back, and the guarantees of the
/// [module docs](self) hold per observer: a panic in one observer's call
/// is caught and logged, the remaining observers are still called, and the
/// panicking observer's `on_reply` still runs (with `tag = None` after a
/// panicking `on_transact`). The `elapsed` each `on_reply` receives
/// excludes every observer's `on_transact`.
#[derive(Clone)]
pub struct Fanout {
    observers: Vec<Arc<dyn TransactionObserver>>,
}

impl Fanout {
    /// Forward to `observers`, in iteration order.
    pub fn new(observers: impl IntoIterator<Item = Arc<dyn TransactionObserver>>) -> Self {
        Self {
            observers: observers.into_iter().collect(),
        }
    }

    /// The observers this forwards to, in call order.
    pub fn observers(&self) -> &[Arc<dyn TransactionObserver>] {
        &self.observers
    }
}

impl std::fmt::Debug for Fanout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fanout")
            .field("observers", &self.observers.len())
            .finish()
    }
}

struct FanoutTags(Vec<Option<Box<dyn Any + Send>>>);

impl TransactionObserver for Fanout {
    fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        let tags = self
            .observers
            .iter()
            .map(|observer| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.on_transact(ctx)))
                    .unwrap_or_else(|payload| {
                        log_panic("Fanout: TransactionObserver::on_transact", &*payload);
                        None
                    })
            })
            .collect();
        Some(Box::new(FanoutTags(tags)))
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        tag: Option<Box<dyn Any + Send>>,
        result: &Result<()>,
        elapsed: Duration,
    ) {
        let mut tags = tag
            .and_then(|tag| tag.downcast::<FanoutTags>().ok())
            .map(|tags| tags.0)
            .unwrap_or_default()
            .into_iter();
        for observer in &self.observers {
            let tag = tags.next().flatten();
            if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                observer.on_reply(ctx, tag, result, elapsed)
            })) {
                log_panic("Fanout: TransactionObserver::on_reply", &*payload);
            }
        }
    }
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
    log::error!("{which} panicked: {msg}");
}

/// Run `dispatch` between the observer's two calls; `ctx` is built only if an observer is set.
pub(crate) fn observed<'a>(
    ctx: impl FnOnce() -> TxnContext<'a>,
    dispatch: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let Some(observer) = current() else {
        return dispatch();
    };
    // `ctx` runs user `transaction_name`/`descriptor` impls; contain them like the observer calls.
    let ctx = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(ctx)) {
        Ok(ctx) => ctx,
        Err(payload) => {
            log_panic("TxnContext (transaction_name/descriptor)", &*payload);
            return dispatch();
        }
    };
    let tag = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observer.on_transact(&ctx)))
        .unwrap_or_else(|payload| {
            log_panic("TransactionObserver::on_transact", &*payload);
            None
        });
    let start = Instant::now();
    let result = dispatch();
    let elapsed = start.elapsed();
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        observer.on_reply(&ctx, tag, &result, elapsed)
    })) {
        log_panic("TransactionObserver::on_reply", &*payload);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StatusCode;
    use std::sync::Mutex;

    fn ctx() -> TxnContext<'static> {
        TxnContext::new("x.y.IZ", 7)
            .with_method("m")
            .with_calling_uid(1)
            .with_calling_pid(2)
    }

    #[test]
    fn txn_context_new_sets_defaults_and_with_sets_each_field() {
        let c = TxnContext::new("a.b.IC", 3);
        assert_eq!(
            (c.descriptor, c.code, c.method, c.is_oneway),
            ("a.b.IC", 3, None, false)
        );
        assert_eq!((c.calling_uid, c.calling_pid), (0, 0));
        assert!(c.transport == TransportCaps::KERNEL);

        let c = c
            .with_method("m")
            .with_oneway(true)
            .with_calling_uid(10)
            .with_calling_pid(20)
            .with_transport(TransportCaps::SAME_HOST);
        assert_eq!(
            (c.descriptor, c.code, c.method, c.is_oneway),
            ("a.b.IC", 3, Some("m"), true)
        );
        assert_eq!((c.calling_uid, c.calling_pid), (10, 20));
        assert!(c.transport == TransportCaps::SAME_HOST);
    }

    #[test]
    #[serial_test::serial(observer)]
    fn observer_getter_replace_and_fanout_keep_each_tag() {
        let as_dyn = |r: &Arc<Recorder>| -> Arc<dyn TransactionObserver> { r.clone() };
        let plain = Arc::new(Recorder::default());
        let bad_transact = Arc::new(Recorder {
            panic_in: Some("on_transact"),
            ..Default::default()
        });
        let bad_reply = Arc::new(Recorder {
            panic_in: Some("on_reply"),
            ..Default::default()
        });

        assert!(replace_observer(Some(as_dyn(&plain))).is_none());
        let installed = observer().expect("the getter sees the installed observer");
        assert!(Arc::ptr_eq(&installed, &as_dyn(&plain)));

        let fanout: Arc<dyn TransactionObserver> = Arc::new(Fanout::new([
            as_dyn(&bad_transact),
            as_dyn(&plain),
            as_dyn(&bad_reply),
        ]));
        let previous = replace_observer(Some(fanout.clone())).expect("the replaced observer");
        assert!(Arc::ptr_eq(&previous, &installed));
        drop((previous, installed));

        assert_eq!(observed(ctx, || Ok(())), Ok(()));
        assert_eq!(
            *bad_transact.events.lock().unwrap(),
            ["transact 7", "reply 7 None Ok(())"],
            "a panicking on_transact costs only its own tag"
        );
        for r in [&plain, &bad_reply] {
            assert_eq!(
                *r.events.lock().unwrap(),
                ["transact 7", "reply 7 Some(42) Ok(())"]
            );
        }

        let last = replace_observer(None).expect("the fanout");
        assert!(Arc::ptr_eq(&last, &fanout));
        assert!(observer().is_none());
    }

    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<String>>,
        panic_in: Option<&'static str>,
    }

    // Parallel lib tests dispatch real transactions; record only this test's context.
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

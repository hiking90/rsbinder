// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Cooperative cancellation, the way AOSP does it
//! (`android.os.ICancellationSignal`).
//!
//! Kernel binder has no per-call deadline and no way to abort a
//! transaction in flight, so a long-running call is cancelled the same
//! way the Android framework cancels one: the **service** creates a
//! signal, hands its transport binder to the caller, and the caller
//! sends a `oneway cancel()` to that binder. The service's own work loop
//! is what notices — nothing interrupts it from outside.
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::cancel::{CancellationSignal, ICancellationSignal};
//! // Service: make one per operation and give the caller its transport.
//! let signal = CancellationSignal::new();
//! let transport: Strong<dyn ICancellationSignal> = signal.create_transport();
//! // ... return `transport` from an AIDL method, then, in the work loop:
//! let token = signal.token();
//! for _item in 0..1000 {
//!     if token.is_canceled() {
//!         break;
//!     }
//! }
//! ```
//!
//! ```no_run
//! # use rsbinder::*;
//! # fn f(transport: &SIBinder) -> Result<()> {
//! // Caller: cancel through the binder the service handed back.
//! rsbinder::cancel::cancel_remote(transport)?;
//! # Ok(())
//! # }
//! ```
//!
//! **Direction.** Only this one is supported: the service creates, the
//! caller cancels. The mirror image — a caller creating a signal and the
//! service polling it — makes every poll a transaction back to the
//! caller, which on the RPC stack additionally requires the client to
//! have opened incoming connections
//! ([`TransportCaps::CALLBACKS`](crate::TransportCaps::CALLBACKS)).
//!
//! **What cancellation means is the service's to decide.** This module
//! carries the notification and nothing else; AOSP likewise leaves the
//! outcome to the service, which throws `OperationCanceledException` of
//! its own accord. A service that stops early typically returns a
//! service-specific error saying so (see
//! [`Status::service_specific`](crate::Status::service_specific)).
//!
//! **Delivery is best-effort, and unordered against your other calls.**
//! `cancel()` is `oneway`, so on kernel binder it queues on the
//! transport node's async list: if the operation being cancelled is
//! occupying the whole thread pool, the cancel arrives when a thread
//! frees up. More surprising in practice — the transport is a *different
//! binder object* from the service, and the kernel orders a oneway to
//! one node against a twoway to another not at all. A caller that
//! cancels and then asks the service what it saw can be answered before
//! the cancel is delivered; measured, that is not rare. Poll, or have
//! the service report through the result of the operation being
//! cancelled. (One RPC session serializes its transactions, so the same
//! sequence happens to be ordered there — do not build on that.)
//!
//! Spelling follows AOSP — `canceled` with one `l`, as in
//! `CancellationSignal.isCanceled()` — everywhere in this module,
//! including the async [`CancellationToken::canceled`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::binder::{FromIBinder, Interface, Strong};
use crate::error::{Result, StatusCode};
use crate::status::BinderResult;
use crate::SIBinder;

include!(concat!(env!("OUT_DIR"), "/cancellation_signal.rs"));

pub use android::os::ICancellationSignal::{
    BnCancellationSignal, BpCancellationSignal, ICancellationSignal, ICancellationSignalDefault,
    ICancellationSignalDefaultRef,
};

#[cfg(feature = "async")]
pub use android::os::ICancellationSignal::{
    ICancellationSignalAsync, ICancellationSignalAsyncService,
};

/// Shared state behind a [`CancellationSignal`], its [`CancellationToken`]s,
/// and the transport binder. Held by `Arc` from all three so a service
/// that keeps only the transport alive still cancels the work.
struct Inner {
    canceled: AtomicBool,
    /// Taken out before it is called, which is both how "at most once"
    /// is enforced and how the lock is kept off the user's callback (R6).
    on_cancel: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Inner {
    fn new() -> Arc<Self> {
        Arc::new(Inner {
            canceled: AtomicBool::new(false),
            on_cancel: Mutex::new(None),
            #[cfg(feature = "tokio")]
            notify: tokio::sync::Notify::new(),
        })
    }

    fn cancel(self: &Arc<Self>) {
        // `swap` rather than `store`: a second cancel — from a retrying
        // client, or from the service itself after the client already
        // cancelled — must not run the callback again.
        if self.canceled.swap(true, Ordering::SeqCst) {
            return;
        }
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
        let listener = self
            .on_cancel
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        // Outside the lock: this runs on a binder worker thread, and the
        // callback is user code that may call back into this signal.
        if let Some(listener) = listener {
            listener();
        }
    }

    fn set_on_cancel(self: &Arc<Self>, f: Box<dyn FnOnce() + Send>) {
        if self.canceled.load(Ordering::SeqCst) {
            // Already cancelled: run it now rather than never, matching
            // AOSP's `setOnCancelListener`.
            f();
            return;
        }
        let previous = {
            let mut slot = self.on_cancel.lock().unwrap_or_else(|e| e.into_inner());
            slot.replace(f)
        };
        drop(previous);
        // Lost the race with `cancel()`: it took the slot before this
        // one was in it, so nothing would ever run what was just stored.
        if self.canceled.load(Ordering::SeqCst) {
            let late = self
                .on_cancel
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(late) = late {
                late();
            }
        }
    }
}

/// The transport binder a service hands to its caller: an
/// `android.os.ICancellationSignal` whose `cancel()` cancels the signal
/// it was made from.
struct Transport(Arc<Inner>);

impl Interface for Transport {}

impl ICancellationSignal for Transport {
    fn r#cancel(&self) -> BinderResult<()> {
        self.0.cancel();
        Ok(())
    }
}

/// A service-side cancellation signal: one per cancellable operation.
///
/// Create it, hand [`create_transport`](Self::create_transport) to the
/// caller, and watch it with [`token`](Self::token),
/// [`is_canceled`](Self::is_canceled) or
/// [`set_on_cancel`](Self::set_on_cancel). See the [module
/// docs](self) for the shape of the whole exchange.
///
/// Cloning shares one state, so a clone cancels the original.
#[derive(Clone)]
pub struct CancellationSignal(Arc<Inner>);

impl Default for CancellationSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationSignal {
    /// A fresh, uncancelled signal.
    pub fn new() -> Self {
        CancellationSignal(Inner::new())
    }

    /// The binder to hand to the caller — AOSP's
    /// `CancellationSignal.createTransport()`.
    ///
    /// Return it from an AIDL method declared to return
    /// `android.os.ICancellationSignal`, or as a bare `IBinder`. Each
    /// call makes a new binder over the same signal; one is the usual
    /// number.
    ///
    /// The returned binder keeps the signal's state alive on its own, so
    /// a service may drop its `CancellationSignal` and keep only a
    /// [`CancellationToken`] — the caller's `cancel()` still arrives.
    pub fn create_transport(&self) -> Strong<dyn ICancellationSignal> {
        BnCancellationSignal::new_binder(Transport(self.0.clone()))
    }

    /// Cancel from the service side, as if the caller had.
    ///
    /// Idempotent: the second call does nothing, and in particular does
    /// not run an [`on_cancel`](Self::set_on_cancel) callback twice.
    pub fn cancel(&self) {
        self.0.cancel();
    }

    /// Whether cancellation has been requested.
    pub fn is_canceled(&self) -> bool {
        self.0.canceled.load(Ordering::SeqCst)
    }

    /// Run `f` when cancellation arrives — or right now, on this thread,
    /// if it already has (AOSP `setOnCancelListener` does the same).
    ///
    /// At most one callback is registered; setting a second replaces the
    /// first, and the replaced one is dropped without running. A
    /// registered callback runs **once**.
    ///
    /// It runs on whatever thread delivers the cancel — a binder worker
    /// for a remote caller — with no lock of this signal held, so it may
    /// call back into the signal. Keep it short: that thread is not
    /// serving other transactions while it runs.
    pub fn set_on_cancel(&self, f: impl FnOnce() + Send + 'static) {
        self.0.set_on_cancel(Box::new(f));
    }

    /// A handle for the code doing the work.
    pub fn token(&self) -> CancellationToken {
        CancellationToken(self.0.clone())
    }
}

impl std::fmt::Debug for CancellationSignal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationSignal")
            .field("canceled", &self.is_canceled())
            .finish()
    }
}

/// The read side of a [`CancellationSignal`], for the code doing the
/// cancellable work.
///
/// `Clone` + `Send` + `Sync`, so one operation's workers can each hold
/// one.
#[derive(Clone)]
pub struct CancellationToken(Arc<Inner>);

impl CancellationToken {
    /// Whether cancellation has been requested.
    ///
    /// The synchronous way to use a token is to check this between units
    /// of work. There is deliberately no blocking `wait`: a thread
    /// parked on a cancel is a binder worker not serving transactions,
    /// and the cancel may never come.
    pub fn is_canceled(&self) -> bool {
        self.0.canceled.load(Ordering::SeqCst)
    }

    /// Completes when cancellation is requested, immediately if it
    /// already has been.
    ///
    /// ```no_run
    /// # use rsbinder::cancel::CancellationToken;
    /// # async fn work(token: CancellationToken) {
    /// // A watcher task beside the work, for a service that has
    /// // something to tear down when the caller gives up.
    /// tokio::spawn(async move {
    ///     token.canceled().await;
    ///     // ... stop the operation
    /// });
    /// # }
    /// ```
    ///
    /// With `tokio/macros` on in your own crate, `tokio::select!` over
    /// this and the work itself is the more direct spelling. rsbinder's
    /// `tokio` feature does not enable that macro for you.
    #[cfg(feature = "tokio")]
    pub async fn canceled(&self) {
        // Register before the check: `notify_waiters` only wakes waiters
        // that already exist, so checking first would drop a cancel that
        // lands in between.
        let notified = self.0.notify.notified();
        if self.is_canceled() {
            return;
        }
        notified.await;
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("canceled", &self.is_canceled())
            .finish()
    }
}

/// Cancel through a transport binder a service handed back.
///
/// The caller's half of the exchange, in one line. `binder` must be an
/// `android.os.ICancellationSignal`; anything else is
/// [`StatusCode::BadType`].
///
/// A method declared in `.aidl` as returning `ICancellationSignal` gives
/// the caller a typed proxy it can call `cancel()` on directly — but
/// that proxy is the *calling crate's* generated type, since every crate
/// that compiles the AIDL gets its own (the wire is the same either
/// way). Taking the transport as a bare `IBinder` and cancelling it with
/// this function avoids compiling a second copy.
pub fn cancel_remote(binder: &SIBinder) -> Result<()> {
    let signal: Strong<dyn ICancellationSignal> =
        FromIBinder::try_from(binder.clone()).map_err(|e| {
            log::error!("cancel_remote: not an android.os.ICancellationSignal: {e:?}");
            StatusCode::BadType
        })?;
    signal.r#cancel().map_err(StatusCode::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Plan 10-5 AC-5.2. No transport here: the signal's own behavior is
    /// what these pin, and it is the same whichever side cancels.
    #[test]
    fn a_listener_runs_once_and_late_registration_runs_now() {
        let signal = CancellationSignal::new();
        let runs = Arc::new(AtomicBool::new(false));

        let flag = runs.clone();
        signal.set_on_cancel(move || flag.store(true, Ordering::SeqCst));
        assert!(!signal.is_canceled());
        assert!(!runs.load(Ordering::SeqCst));

        signal.cancel();
        assert!(signal.is_canceled());
        assert!(runs.load(Ordering::SeqCst), "the listener must have run");

        // A second cancel must not run it again — the listener was taken
        // out of the slot, so a re-run would need a second one.
        runs.store(false, Ordering::SeqCst);
        signal.cancel();
        assert!(!runs.load(Ordering::SeqCst), "the listener ran twice");

        // Registering on an already-cancelled signal runs on this thread
        // rather than never.
        let late = Arc::new(AtomicBool::new(false));
        let flag = late.clone();
        signal.set_on_cancel(move || flag.store(true, Ordering::SeqCst));
        assert!(late.load(Ordering::SeqCst));
    }

    /// A clone and a token see one state, and the transport's `cancel()`
    /// — what a remote caller's transaction ends up calling — is what
    /// drives it.
    #[test]
    fn the_transport_cancels_the_signal_it_came_from() {
        let signal = CancellationSignal::new();
        let token = signal.token();
        let clone = signal.clone();
        let transport = signal.create_transport();

        assert!(!token.is_canceled());
        transport.r#cancel().expect("local transport call");

        assert!(signal.is_canceled());
        assert!(clone.is_canceled());
        assert!(token.is_canceled());
    }

    /// The signal's state outlives the `CancellationSignal` itself, so a
    /// service may hand out the transport, keep a token, and drop the
    /// rest.
    #[test]
    fn a_dropped_signal_still_cancels_through_its_transport() {
        let signal = CancellationSignal::new();
        let token = signal.token();
        let transport = signal.create_transport();
        drop(signal);

        transport.r#cancel().expect("local transport call");
        assert!(token.is_canceled());
    }

    /// Plan 10-5 AC-5.3.
    ///
    /// Bounded with a channel rather than `tokio::time::timeout`: this
    /// crate's `tokio` feature deliberately omits `tokio/time`, and an
    /// unbounded wait would hang the suite instead of failing it.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_async_waiter_wakes_on_cancel_and_returns_at_once_afterwards() {
        use std::sync::mpsc;
        use std::time::Duration;

        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .build()
            .expect("runtime");
        let signal = CancellationSignal::new();
        let token = signal.token();

        // The worker threads run this even while the test thread blocks
        // below, so the wake is observed from outside the runtime.
        let (tx, rx) = mpsc::channel();
        rt.spawn({
            let token = token.clone();
            async move {
                token.canceled().await;
                let _ = tx.send(());
            }
        });
        // No synchronization with the task reaching its await on
        // purpose: cancelling before it gets there is exactly the race
        // the register-before-check in `canceled` has to survive.
        signal.cancel();
        rx.recv_timeout(Duration::from_secs(5))
            .expect("canceled() did not wake within 5s");

        // Already cancelled, and no `notify_waiters` left to come: this
        // can only return by way of the pre-check.
        let (tx, rx) = mpsc::channel();
        rt.spawn(async move {
            token.canceled().await;
            let _ = tx.send(());
        });
        rx.recv_timeout(Duration::from_secs(5))
            .expect("canceled() on an already-cancelled token must return at once");
    }
}

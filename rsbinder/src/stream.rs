// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Streaming a sequence of values over binder, with back-pressure.
//!
//! A binder method returns one value. A method that would return a
//! million rows either builds the whole list in memory on both sides or
//! invents its own paging protocol. This module is the third option: the
//! consumer hands the producer a **sink** to push batches into, and
//! grants credit back as it drains them, so the producer can only run as
//! far ahead as the consumer allows.
//!
//! The contract is two ordinary `.aidl` interfaces,
//! `rsbinder.stream.IStreamSink` and `rsbinder.stream.IStreamSource`
//! (`rsbinder/aidl/stream/`), so a C++ or Java peer can be either end
//! without rsbinder on its side. Their shape is the reactive-streams one
//! — `onStart` / `onBatch` / `onEnd` one way, `request` / `cancel` the
//! other — and every call in it is `oneway`, so neither end ever waits
//! on the other's threads.
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::stream::Sink;
//! # #[derive(Default)] struct Row;
//! # impl Serialize for Row { fn serialize(&self, _p: &mut Parcel) -> Result<()> { Ok(()) } }
//! # fn rows() -> Vec<Row> { Vec::new() }
//! # fn in_a_handler(sink_binder: &SIBinder) -> Result<()> {
//! // Service: called with the consumer's sink. The method itself has
//! // nothing to return — the stream's own calls carry everything.
//! let mut sink = Sink::<Row>::new(sink_binder)?;
//! std::thread::spawn(move || {
//!     for row in rows() {
//!         // Blocks here once the granted window is used up.
//!         if sink.send(&row).is_err() {
//!             return;
//!         }
//!     }
//!     let _ = sink.end();
//! });
//! Ok(())
//! # }
//! ```
//!
//! An async service does the same from a task, where waiting for credit
//! suspends the task instead of parking a thread:
#![cfg_attr(
    feature = "tokio",
    doc = "[`Sink::send_async`] and [`Sink::end_async`]."
)]
#![cfg_attr(
    not(feature = "tokio"),
    doc = "`Sink::send_async` and `Sink::end_async` (`tokio` feature)."
)]
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::stream::Receiver;
//! # #[derive(Default)] struct Row;
//! # impl Deserialize for Row { fn deserialize(_p: &mut Parcel) -> Result<Self> { Ok(Row) } }
//! # fn subscribe(_sink: &SIBinder) -> Result<()> { unimplemented!() }
//! # fn consume() -> Result<()> {
//! // Consumer: make the receiver, pass its sink, read.
//! let (mut rx, sink_binder) = Receiver::<Row>::new();
//! subscribe(&sink_binder)?;
//! for row in &mut rx {
//!     let _row = row?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! **The producer must be able to call the consumer.** Kernel binder
//! always can. An RPC session can only when the client opened incoming
//! connections, so [`Sink::new`] refuses a sink whose session lacks
//! [`TransportCaps::CALLBACKS`](crate::TransportCaps::CALLBACKS) rather
//! than letting the first batch fail minutes later.
//!
//! # When a batch leaves
//!
//! Two things send one, and no clock is either of them: the pending
//! batch reaching [`DEFAULT_MAX_BATCH_BYTES`] (or whatever
//! [`Sink::with_limits`] was given), and a call to [`Sink::flush`] or
//! [`Sink::end`]. Which of those a producer needs follows from how its
//! items arrive.
//!
//! * **Items already in hand** — a query result, a directory listing, a
//!   file read in chunks. The byte threshold does the batching; `end`
//!   sends the remainder. Nothing else to do.
//! * **Items arriving at their own pace** — an event feed, a sensor, a
//!   log tail. Call `flush` at the point where what has been sent so far
//!   is what the consumer should have now, typically after each event or
//!   each burst. Without it an item sits in the pending batch until the
//!   *next* [`Sink::send`] fills it, which for an idle producer is not a
//!   bounded wait: the consumer stays blocked in [`Receiver::recv`] and
//!   cannot tell that from a producer with nothing to report.
//!   [`Sink::pending`] is what a service checks to confirm that
//!   diagnosis.
//!
//! Flushing after every item costs a batching producer nothing — `send`
//! has already sent them and `flush` on an empty batch returns at once —
//! so a producer unsure which it is can simply flush.
//!
//! **Items carry no binder and no file descriptor.** A batch is bytes
//! with no object table, so an item holding either is refused at
//! [`send`](Sink::send) — see [`crate::to_bytes`], which uses the same
//! parcel mode.
//!
//! **The consumer holds the producer to its credit.** The producer states
//! its opening window when it introduces itself, so the consumer knows all
//! it may send — that window, plus what has been granted since — and ends
//! the stream on a batch beyond it rather than queue without bound. How
//! wide a window it accepts is the consumer's to set
//! ([`Receiver::with_limits`]), because the consumer may be a service and
//! its producer a client it has no reason to trust.
//!
//! **A peer that dies ends the stream.** Back-pressure means that most
//! of the time neither side has a call in flight to fail on, so each
//! watches the other's binder instead: the producer's [`Sink::send`]
//! reports [`StatusCode::DeadObject`] rather than staying parked for
//! credit, and the consumer's [`Receiver::recv`] yields that same error
//! rather than waiting for a batch. The consumer starts watching when the
//! producer's `onStart` arrives, which is before any batch can.
//!
//! **Ordering.** `onStart`, `onBatch` and `onEnd` are all `oneway` to the
//! same object, and both transports keep `oneway` calls to one object in
//! order — the kernel per node, an RPC session per address — so the
//! source is known before the first batch and the terminator cannot
//! overtake the last. Credit runs the other way and is `oneway` too: a
//! grant never waits on the producer's threads, though on the RPC stack
//! the send itself waits for a free outgoing connection on the session.
//!
//! # Async
//!
//! With the `tokio` feature, every `*_async` method makes its binder call
//! from the blocking pool, so a send that waits on the transport does not
//! hold an executor thread. **Poll them inside a Tokio runtime**: outside
//! one the hand-off panics, as `tokio::task::spawn_blocking` does. A call
//! from inside a transaction handler makes its binder call on the calling
//! thread instead and is exempt.

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::binder::{FromIBinder, Interface, Strong};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, Serialize};
use crate::status::{BinderResult, ExceptionCode, Status};
use crate::SIBinder;

// The generated contract is an implementation detail: the public surface
// here is `Sink` and `Receiver`, both of which speak `SIBinder`.
// Exposing the tree would put every generated trait, proxy and stub under
// the crate's stability promise, and a caller that wants the raw
// interface can compile the shipped `.aidl` itself.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stream.rs"));
}

use generated::rsbinder::stream::IStreamSink::{BnStreamSink, BpStreamSink, IStreamSink};
// Only the hand-built request path names a transaction code; the typed
// proxy carries its own.
#[cfg(feature = "rpc")]
use generated::rsbinder::stream::IStreamSink::transactions as sink_transactions;
use generated::rsbinder::stream::IStreamSource::{BnStreamSource, BpStreamSource, IStreamSource};

/// The most bytes one [`onBatch`](Sink::send) call carries before the
/// producer sends it, unless [`Sink::with_limits`] says otherwise.
///
/// Sized against the kernel driver rather than picked for roundness.
/// A process's asynchronous transactions share half its binder mapping
/// (`binder_alloc.c`, `buffer_size / 2`), which is about 512 KB at the
/// default 1 MB mapping, and the driver only begins to look at a single
/// sender for spam once the free part of that space falls below
/// `buffer_size / 10` — about 102 KB. A full default window of
/// [`DEFAULT_CREDIT_WINDOW`] batches of this size is 64 KB in flight,
/// which stays under that line, so one stream never brings the check
/// into play on its own.
pub const DEFAULT_MAX_BATCH_BYTES: usize = 16 * 1024;

/// How many batches a producer may have in flight before it has to wait,
/// unless [`Sink::with_limits`] says otherwise.
///
/// It is also the consumer's default credit window
/// ([`Receiver::with_credit_window`]), which is a different quantity:
/// how many drained batches the consumer lets go unpaid before it grants.
/// The producer's ceiling stays whatever it opened with, because the
/// consumer grants exactly one batch back per batch it drains.
///
/// And it is the widest opening window a consumer accepts unless
/// [`Receiver::with_limits`] says otherwise, so that the two defaults
/// agree: a producer made with [`Sink::new`] is always taken by a
/// consumer made with [`Receiver::new`].
///
/// Four rather than one so the producer is not stalled for a round trip
/// after every batch, and four rather than forty so the bytes in flight
/// stay inside the budget [`DEFAULT_MAX_BATCH_BYTES`] describes. It is
/// also far below the 50 outstanding asynchronous buffers per sender at
/// which the driver starts flagging spam (`binder_alloc.c`).
pub const DEFAULT_CREDIT_WINDOW: u32 = 4;

// ---------------------------------------------------------------------
// Shared credit state (producer side)
// ---------------------------------------------------------------------

/// What the producer waits on and the source object's binder feeds.
#[derive(Default)]
struct CreditState {
    /// Batches the producer may still send.
    available: u64,
    /// The highest running total the consumer has granted. A total not
    /// above it is a grant seen before and adds nothing.
    granted_total: i64,
    /// The consumer asked for no more. Latched: a cancel is never undone.
    canceled: bool,
    /// The consumer's process is gone. Latched, and checked before
    /// `canceled` because it is the more specific answer.
    dead: bool,
}

#[derive(Default)]
struct Credit {
    state: Mutex<CreditState>,
    /// Woken by a grant, a cancel and the consumer's death — each of the
    /// three releases a waiting producer.
    wake: Condvar,
    /// The same three, for a producer suspended in
    /// [`wait_credit_async`](Self::wait_credit_async).
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Credit {
    fn lock(&self) -> std::sync::MutexGuard<'_, CreditState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Credit from this side: the opening window, or one given back.
    fn grant(&self, credits: u32) {
        {
            let mut state = self.lock();
            state.available = state.available.saturating_add(credits as u64);
        }
        self.wake_all();
    }

    /// The consumer's running total. Only the part above the highest seen
    /// so far is new, which is what makes a repeated grant harmless.
    fn granted_up_to(&self, total: i64) {
        {
            let mut state = self.lock();
            if total <= state.granted_total {
                return;
            }
            let gained = (total - state.granted_total) as u64;
            state.granted_total = total;
            state.available = state.available.saturating_add(gained);
        }
        self.wake_all();
    }

    fn cancel(&self) {
        self.lock().canceled = true;
        self.wake_all();
    }

    fn mark_dead(&self) {
        self.lock().dead = true;
        self.wake_all();
    }

    fn wake_all(&self) {
        self.wake.notify_all();
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
    }

    /// One step of taking a credit: the answer if there is one now,
    /// `None` if the caller has to wait.
    fn poll_credit(state: &mut CreditState) -> Option<Result<()>> {
        if state.dead {
            return Some(Err(StatusCode::DeadObject));
        }
        if state.canceled {
            return Some(Err(StatusCode::InvalidOperation));
        }
        if state.available > 0 {
            state.available -= 1;
            return Some(Ok(()));
        }
        None
    }

    fn is_canceled(&self) -> bool {
        self.lock().canceled
    }

    /// Take one credit if one is there. For [`Sink`]'s `Drop`, which
    /// cannot park the unwinding thread until a grant arrives.
    fn try_credit(&self) -> bool {
        let mut state = self.lock();
        if state.dead || state.canceled || state.available == 0 {
            return false;
        }
        state.available -= 1;
        true
    }

    /// Take one credit, waiting until one is granted. Death ends the wait
    /// as well as a grant or a cancel: with nothing in flight to fail, a
    /// parked producer has no other way to learn its consumer is gone.
    fn wait_credit(&self) -> Result<()> {
        let mut state = self.lock();
        loop {
            if let Some(answer) = Self::poll_credit(&mut state) {
                return answer;
            }
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// [`wait_credit`](Self::wait_credit) that suspends the task instead
    /// of parking the thread.
    #[cfg(feature = "tokio")]
    async fn wait_credit_async(&self) -> Result<()> {
        loop {
            // Created before the check: a `Notified` receives
            // `notify_waiters` from the moment it exists, so a grant that
            // lands between the check and the await is not missed.
            let notified = self.notify.notified();
            if let Some(answer) = Self::poll_credit(&mut self.lock()) {
                return answer;
            }
            notified.await;
        }
    }
}

/// Watches the consumer's sink so a producer parked on
/// [`Credit::wait_credit`] learns that nobody is there any more.
struct SinkDeath(Arc<Credit>);

impl crate::DeathRecipient for SinkDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        self.0.mark_dead();
    }
}

/// Register `recipient` for `binder`'s death; `Ok(None)` when `binder` is
/// neither a kernel proxy nor an RPC proxy, which are the only two a death
/// link can be put on. That is an object in this process — including a
/// local wrapper standing in for a remote one, whose peer's death this
/// stream therefore does not see. The returned `Arc` is what keeps the
/// link alive — the binder holds only a `Weak`, and dropping it only makes
/// the link inert, so the owner unlinks it (`unlink_death`).
fn watch_death<R>(binder: &SIBinder, recipient: R) -> Result<Option<Arc<dyn crate::DeathRecipient>>>
where
    R: crate::DeathRecipient + 'static,
{
    if binder.as_remote().is_none() {
        return Ok(None);
    }
    let recipient: Arc<dyn crate::DeathRecipient> = Arc::new(recipient);
    // Fatal to the stream: back-pressure leaves nothing in flight to
    // fail, so this link is the only way either end learns the other is
    // gone.
    if let Err(e) = binder.link_to_death(Arc::downgrade(&recipient)) {
        log::error!("stream: cannot watch the peer for death: {e:?}");
        return Err(e);
    }
    Ok(Some(recipient))
}

/// Undo what [`watch_death`] registered. Dropping the recipient leaves a
/// dead `Weak` in the proxy's list, which never shrinks on its own: the
/// kernel subscription stays, and a later `link_to_death` on the same
/// proxy finds the list non-empty and never asks for one. Call it with no
/// lock held — it reaches into the proxy.
fn unlink_death(binder: &SIBinder, recipient: &Option<Arc<dyn crate::DeathRecipient>>) {
    let Some(recipient) = recipient else { return };
    // Two callers are `Drop`: a kernel proxy's `unlink_to_death` panics on
    // a poisoned recipients lock, and a panic leaving `Drop` during an
    // unwind aborts the process (`bridge::unlink_all` catches for this).
    let unlinked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = binder.unlink_to_death(Arc::downgrade(recipient));
    }));
    if unlinked.is_err() {
        log::error!("stream: unlink_to_death panicked; a death link stays registered");
    }
}

/// The object the consumer calls to grant credit — the binder half of
/// [`Credit`].
struct SourceObject(Arc<Credit>);

impl Interface for SourceObject {}

impl IStreamSource for SourceObject {
    fn r#request(&self, total: i64) -> BinderResult<()> {
        if total <= 0 {
            // A total of nothing grants nothing, and a negative one would
            // have to mean "take credit back", which the producer may
            // already have spent. The call is `oneway`, so the log is the
            // only place this can be said. A total seen before is not a
            // mistake — it is a grant sent again — and is passed over in
            // silence by `granted_up_to`.
            log::warn!("stream: ignoring a granted total of {total}");
            return Ok(());
        }
        self.0.granted_up_to(total);
        Ok(())
    }

    fn r#cancel(&self) -> BinderResult<()> {
        self.0.cancel();
        Ok(())
    }
}

// ---------------------------------------------------------------------
// Talking to a binder that may not know its own interface yet
// ---------------------------------------------------------------------

/// A sink or source binder, resolved once. An RPC proxy off the wire
/// carries no descriptor, and a typed cast would stamp one permanently
/// (`OnceLock`) onto the proxy the per-address cache shares, so it is
/// driven by hand as [`cancel_remote`](crate::cancel::cancel_remote)
/// drives one.
enum Peer<I: FromIBinder + ?Sized> {
    Typed(Strong<I>),
    #[cfg(feature = "rpc")]
    Unstamped(SIBinder),
}

/// Resolve `binder` against `I`, refusing a binder that states a
/// different interface.
fn resolve_peer<I>(binder: &SIBinder, expected: &str, what: &str) -> Result<Peer<I>>
where
    I: FromIBinder + ?Sized,
{
    let actual = binder.descriptor();
    if actual.is_empty() {
        #[cfg(feature = "rpc")]
        if (**binder)
            .as_any()
            .downcast_ref::<crate::rpc::RpcProxy>()
            .is_some()
        {
            return Ok(Peer::Unstamped(binder.clone()));
        }
        log::error!("{what}: a descriptor-less binder that is not an RPC proxy");
        return Err(StatusCode::BadType);
    }
    if actual != expected {
        log::error!("{what}: not an {expected}: {actual}");
        return Err(StatusCode::BadType);
    }
    Ok(Peer::Typed(FromIBinder::try_from(binder.clone()).map_err(
        |e| {
            log::error!("{what}: not an {expected}: {e:?}");
            StatusCode::BadType
        },
    )?))
}

/// What the transport under `binder` can do: an RPC proxy answers from
/// its session, anything else is local or kernel and can always be called.
fn peer_caps(binder: &SIBinder) -> crate::TransportCaps {
    #[cfg(feature = "rpc")]
    if let Some(proxy) = (**binder).as_any().downcast_ref::<crate::rpc::RpcProxy>() {
        return proxy.session_caps();
    }
    #[cfg(not(feature = "rpc"))]
    let _ = binder;
    crate::TransportCaps::KERNEL
}

// ---------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------

#[derive(Default)]
struct LedgerState {
    /// Items that left the pending batch and never reached the consumer.
    /// Only the terminator can still report them.
    lost: i32,
    /// A send that failed with nobody told yet; the next call returns it.
    unreported: Option<StatusCode>,
    /// A batch is on its way. Nothing may be sent past it, or it would
    /// arrive out of order.
    in_transit: bool,
    /// A send failed without saying whether the batch arrived, so how
    /// much credit is left can no longer be known. Nothing more is sent.
    broken: Option<StatusCode>,
}

/// What became of the batches that left a [`Sink`], shared with the pool
/// task carrying one: dropping a `*_async` future detaches that task
/// rather than cancelling it, so the task is what writes the outcome here.
#[derive(Default)]
struct Ledger {
    state: Mutex<LedgerState>,
    idle: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Ledger {
    fn lock(&self) -> std::sync::MutexGuard<'_, LedgerState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lost(&self) -> i32 {
        self.lock().lost
    }

    fn add_lost(&self, count: i32) {
        let mut state = self.lock();
        state.lost = state.lost.saturating_add(count);
    }

    fn take_unreported(&self) -> Option<StatusCode> {
        self.lock().unreported.take()
    }

    fn broken(&self) -> Option<StatusCode> {
        self.lock().broken
    }

    /// Block until no batch is in transit.
    fn wait_idle(&self) {
        let mut state = self.lock();
        while state.in_transit {
            state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    #[cfg(feature = "tokio")]
    async fn idle_async(&self) {
        loop {
            // Created before the check, as in `wait_credit_async`.
            let notified = self.notify.notified();
            if !self.lock().in_transit {
                return;
            }
            notified.await;
        }
    }
}

/// What became of a batch, as far as the sender can tell.
enum Outcome {
    /// Never handed to the transport, so it did not arrive.
    Unsent(StatusCode),
    Delivered,
    /// The transport refused it, so it did not arrive.
    Refused(StatusCode),
    /// The send failed — or unwound — without saying which.
    Unknown(StatusCode),
}

/// Whether a `oneway` send that failed with `e` is known not to have
/// arrived. The first two are the driver refusing the transaction, which
/// it does before queueing anything (`binder.c`,
/// `err_dead_proc_or_thread`); any other kernel failure may be an
/// unrelated command that went wrong while this thread waited for its
/// `BR_TRANSACTION_COMPLETE`, with the batch already accepted. Over RPC a
/// send deadline means a frame that did not go out whole, which the peer
/// cannot dispatch — and one that expired before the first byte leaves
/// the connection serving, so a stream broken by it would stay broken on
/// a session that is fine.
fn certainly_not_delivered(e: StatusCode, over_rpc: bool) -> bool {
    matches!(e, StatusCode::FailedTransaction | StatusCode::DeadObject)
        || (over_rpc && e == StatusCode::TimedOut)
}

/// One batch between the pending buffer and the consumer, holding the
/// credit taken for it. Delivered, refused, dropped by a pool task that
/// never ran, or unwound past — every way out goes through `Drop`, which
/// is where an undelivered batch gives its credit back and is counted.
struct BatchInTransit {
    credit: Arc<Credit>,
    ledger: Arc<Ledger>,
    bytes: Vec<u8>,
    count: i32,
    outcome: Outcome,
}

impl BatchInTransit {
    /// The caller has taken one credit for `count` items.
    fn new(credit: Arc<Credit>, ledger: Arc<Ledger>, count: i32) -> Self {
        ledger.lock().in_transit = true;
        BatchInTransit {
            credit,
            ledger,
            bytes: Vec::new(),
            count,
            // What a pool task that never runs leaves it as.
            outcome: Outcome::Unsent(StatusCode::FailedTransaction),
        }
    }

    fn send(mut self, peer: &Peer<dyn IStreamSink>) {
        // What an unwind out of the call leaves it as.
        self.outcome = Outcome::Unknown(StatusCode::Unknown);
        self.outcome = match peer.on_batch(&self.bytes, self.count) {
            Ok(()) => Outcome::Delivered,
            Err(e) if certainly_not_delivered(e, peer.over_rpc()) => Outcome::Refused(e),
            Err(e) => Outcome::Unknown(e),
        };
    }
}

impl Drop for BatchInTransit {
    fn drop(&mut self) {
        // The failure, and whether the batch is known not to have arrived.
        let (failure, not_delivered) = match self.outcome {
            Outcome::Delivered => (None, false),
            Outcome::Unsent(e) | Outcome::Refused(e) => (Some(e), true),
            Outcome::Unknown(e) => (Some(e), false),
        };
        if not_delivered {
            // The consumer will never grant back the credit this batch
            // took. The items are not resent: they are counted as lost,
            // and the stream ends saying so.
            self.credit.grant(1);
        }
        {
            let mut state = self.ledger.lock();
            if let Some(e) = failure {
                state.lost = state.lost.saturating_add(self.count);
                state.unreported = Some(e);
                // Given back, the credit would be one the consumer never
                // issued if the batch did arrive, and the consumer ends a
                // stream on a batch sent without credit. Kept, it would
                // shrink the window for good if the batch did not.
                if !not_delivered {
                    state.broken = Some(e);
                }
            }
            // In the same critical section, so a waiter that sees the
            // batch gone sees what became of it.
            state.in_transit = false;
        }
        self.ledger.idle.notify_all();
        #[cfg(feature = "tokio")]
        self.ledger.notify.notify_waiters();
    }
}

/// The terminator on its way to the blocking pool. A pool task has the
/// same ways out as a batch, and a terminator that goes nowhere leaves the
/// consumer blocked with both processes alive — so one dropped unsent is
/// sent from wherever the drop happens.
#[cfg(feature = "tokio")]
struct TerminatorInTransit {
    peer: Arc<Peer<dyn IStreamSink>>,
    exception: i32,
    service_specific: i32,
    message: Option<String>,
    /// What the send came to, wherever it was made from: the pool's own
    /// answer says only whether the pool ran the task.
    outcome: Arc<Mutex<Option<Result<()>>>>,
    sent: bool,
}

#[cfg(feature = "tokio")]
impl TerminatorInTransit {
    fn send(mut self) {
        self.deliver();
    }

    fn deliver(&mut self) {
        // Before the call: a send that was tried is not tried again.
        self.sent = true;
        let sent = self.peer.on_end(
            self.exception,
            self.service_specific,
            self.message.as_deref(),
        );
        if let Err(e) = sent {
            log::warn!("stream: the terminator could not be delivered: {e:?}");
        }
        *self.outcome.lock().unwrap_or_else(|e| e.into_inner()) = Some(sent);
    }
}

#[cfg(feature = "tokio")]
impl Drop for TerminatorInTransit {
    fn drop(&mut self) {
        if !self.sent {
            self.deliver();
        }
    }
}

/// The producer's handle on a stream: write items in, they leave in
/// batches.
///
/// Made by [`Sink::new`] from the sink binder the consumer supplied.
/// Values of `T` are encoded with the same codec as [`crate::to_bytes`],
/// so the consumer's `T` need only agree on the wire, not be the same
/// Rust type.
///
/// Every method that produces takes `&mut self`, so one thread or task at
/// a time sends and the pending batch needs no lock of its own. Moving
/// the sink to a worker thread after construction is the usual shape —
/// see the [module docs](self) — and with the `tokio` feature the
/// `*_async` methods do the same from a task.
///
/// Call [`end`](Self::end) on every path, including the failing ones.
/// A `Sink` dropped without it still flushes what it can and still
/// terminates the stream — its `Drop` does, and documents the limits —
/// but the stream is reported as having ended badly, because from the
/// consumer's side that is all a vanished producer can mean.
pub struct Sink<T: ?Sized> {
    /// Shared so an async send can move it onto the blocking pool.
    peer: Arc<Peer<dyn IStreamSink>>,
    credit: Arc<Credit>,
    /// Items encoded so far, awaiting a batch send.
    batch: Parcel,
    count: i32,
    max_batch_bytes: usize,
    /// Set once a terminator has gone out, so `Drop` does not send a
    /// second one.
    ended: bool,
    /// What became of the batches that left; shared with a pool task.
    ledger: Arc<Ledger>,
    /// The object the consumer grants credit on. Held here so it lives
    /// exactly as long as the producer does; the consumer got its own
    /// reference in `onStart`.
    _source: SIBinder,
    /// The consumer's sink, kept for the unlink in `Drop`.
    sink: SIBinder,
    /// Holds the death link on the sink; the binder keeps only a `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    _item: PhantomData<fn(&T)>,
}

impl<T: Serialize + ?Sized> Sink<T> {
    /// Take the consumer's sink and start a stream.
    ///
    /// `sink` is the binder the consumer produced with
    /// [`Receiver::new`], declared in `.aidl` as
    /// `rsbinder.stream.IStreamSink` or as a bare `IBinder`.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::InvalidOperation`] when the transport cannot carry
    ///   a call to the consumer outside a handler. On the RPC stack that
    ///   means the client did not open incoming connections
    ///   ([`ClientOptions::incoming_connections`](crate::ClientOptions::incoming_connections));
    ///   the log line says so. Kernel binder never fails this way.
    /// - [`StatusCode::BadType`] when `sink` states some other interface.
    ///   A binder with no interface yet — an RPC proxy fresh off the wire
    ///   — cannot be checked and is accepted; a wrong one then shows up
    ///   as the consumer's server refusing the interface token.
    /// - Whatever watching the consumer for death, or sending `onStart`,
    ///   fails with — [`StatusCode::DeadObject`] for a consumer that is
    ///   already gone.
    pub fn new(sink: &SIBinder) -> Result<Self> {
        Self::with_limits(sink, DEFAULT_MAX_BATCH_BYTES, DEFAULT_CREDIT_WINDOW)
    }

    /// [`new`](Self::new) with the batch cap and the opening window set
    /// explicitly.
    ///
    /// `initial_credits` is what the producer may send before the
    /// consumer grants anything, so that a short stream finishes without
    /// a credit round trip at all. It is stated to the consumer in
    /// `onStart`, which holds the producer to it and ends the stream at
    /// once when it is more than the consumer accepts — the default
    /// [`Receiver`] accepts [`DEFAULT_CREDIT_WINDOW`], so a wider window
    /// needs a consumer made with [`Receiver::with_limits`].
    ///
    /// `max_batch_bytes` is a threshold, not a hard cap — an item larger
    /// than it still goes out, with the batch it was added to. Keep
    /// `initial_credits * max_batch_bytes` within the budget
    /// [`DEFAULT_MAX_BATCH_BYTES`] describes, or the driver starts
    /// counting this stream as a spam suspect.
    ///
    /// # Errors
    ///
    /// Everything [`new`](Self::new) returns, plus
    /// [`StatusCode::BadValue`] for `initial_credits` of zero, or too
    /// large for the `int` that carries it. The consumer only grants
    /// credit for batches it has taken, so a producer that starts with
    /// none would wait for a grant that nothing can trigger. A window the
    /// consumer refuses is not an error here — `onStart` is `oneway` —
    /// but a cancel, which [`send`](Self::send) reports when it next has
    /// a batch to send; the reason is on the consumer's side.
    pub fn with_limits(
        sink: &SIBinder,
        max_batch_bytes: usize,
        initial_credits: u32,
    ) -> Result<Self> {
        let Some(declared) = i32::try_from(initial_credits).ok().filter(|n| *n > 0) else {
            log::error!("Sink::with_limits: the opening window must be 1..=i32::MAX batches");
            return Err(StatusCode::BadValue);
        };
        peer_caps(sink).require(crate::TransportCaps::CALLBACKS, "a streaming sink")?;
        let peer = resolve_peer::<dyn IStreamSink>(
            sink,
            <BpStreamSink as crate::Proxy>::descriptor(),
            "Sink::new",
        )?;
        let credit = Arc::new(Credit::default());
        credit.grant(initial_credits);
        let source = BnStreamSource::new_binder(SourceObject(credit.clone())).as_binder();
        let death = watch_death(sink, SinkDeath(credit.clone()))?;
        // Before anything else can be sent, so the consumer knows where
        // to grant and whom to watch by the time the first batch lands.
        if let Err(e) = peer.on_start(&source, declared) {
            // No `Sink` is built, so nothing else will undo the link.
            unlink_death(sink, &death);
            return Err(e);
        }
        Ok(Sink {
            peer: Arc::new(peer),
            credit,
            batch: Parcel::new_data_only(),
            count: 0,
            max_batch_bytes: max_batch_bytes.max(1),
            ended: false,
            ledger: Arc::new(Ledger::default()),
            _source: source,
            sink: sink.clone(),
            death,
            _item: PhantomData,
        })
    }

    /// Add one item to the stream.
    ///
    /// The item is encoded now and queued; it leaves once the pending
    /// batch reaches the byte threshold, or at [`flush`](Self::flush) or
    /// [`end`](Self::end). **This call blocks** when a batch is ready to
    /// go and the consumer has granted no credit for it — that is the
    /// back-pressure, and it is why a `Sink` belongs on a thread of its
    /// own rather than on a binder worker.
    ///
    /// Nothing else sends for you. A producer that goes quiet with items
    /// queued leaves them queued, and the consumer, which is blocked in
    /// [`Receiver::recv`], cannot tell that from a producer with nothing
    /// to say yet. Call [`flush`](Self::flush) whenever the items so far
    /// are what the consumer should have now; see the [module
    /// docs](self#when-a-batch-leaves) for which producers have to.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::InvalidOperation`] once the consumer has
    ///   cancelled. Nothing further will be delivered, so stop.
    /// - [`StatusCode::DeadObject`] when the consumer's process is gone.
    /// - [`StatusCode::BadType`] / [`StatusCode::FdsNotAllowed`] for an
    ///   item holding a binder or a file descriptor. The already-queued
    ///   items are unaffected and still go out.
    /// - Anything else is the batch send itself failing, and that batch's
    ///   items are lost: [`end`](Self::end) tells the consumer how many
    ///   went missing, so the stream ends as failed either way. Whether
    ///   it goes on until then depends on what is known of the batch.
    ///   [`StatusCode::FailedTransaction`] is the driver refusing the
    ///   call — a consumer whose buffer for `oneway` calls is full, which
    ///   a busy one can be — and a refused call did not arrive; nor did a
    ///   batch whose send ran into a deadline on an RPC session
    ///   ([`StatusCode::TimedOut`]). For those the credit comes back and
    ///   the stream stays usable. Any other failure leaves it unknown
    ///   whether the batch arrived, and with it how much credit is left;
    ///   the consumer ends a stream on a batch sent without credit, so
    ///   nothing more is sent, and every later [`send`](Self::send) and
    ///   [`flush`](Self::flush) returns the same error without queueing
    ///   anything.
    pub fn send(&mut self, item: &T) -> Result<()> {
        // Before the item is queued: nothing will send it.
        self.usable()?;
        if self.encode(item)? {
            self.flush()?;
        }
        Ok(())
    }

    /// The error that broke the stream, if one has: see `LedgerState::broken`.
    fn usable(&self) -> Result<()> {
        self.ledger.broken().map_or(Ok(()), Err)
    }

    /// Queue one item. `true` when the batch has reached its threshold
    /// and should go out.
    fn encode(&mut self, item: &T) -> Result<bool> {
        let mark = self.batch.data_size();
        if let Err(e) = self.batch.write(item) {
            // A failed encode may have written part of the item. Cut the
            // batch back to what the last successful `send` left, so the
            // items already accepted are still delivered intact.
            self.truncate_batch(mark)?;
            return Err(e);
        }
        self.count += 1;
        Ok(self.batch.data_size() >= self.max_batch_bytes)
    }

    /// [`send`](Self::send) for each item in turn, stopping at the first
    /// failure.
    pub fn send_all<'a, I>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a T>,
        T: 'a,
    {
        for item in items {
            self.send(item)?;
        }
        Ok(())
    }

    /// Send whatever is queued, even if the batch is not full.
    ///
    /// Blocks for credit when there is something to send. Does nothing,
    /// and cannot block, when nothing is queued, so calling it after
    /// every item costs a producer that fills batches anyway nothing —
    /// [`send`](Self::send) has already sent them.
    ///
    /// This is the whole of the timing contract: batches leave on a byte
    /// threshold and on this call, never on a clock. A producer whose
    /// items arrive at their own pace decides here when the consumer
    /// sees them.
    pub fn flush(&mut self) -> Result<()> {
        self.usable()?;
        if self.count == 0 {
            return Ok(());
        }
        // A batch a dropped future left on the pool goes out first, or
        // this one overtakes it; a failure that future could not report
        // is this call's to return, before it sends anything more.
        self.ledger.wait_idle();
        self.reported()?;
        self.credit.wait_credit()?;
        if let Some(batch) = self.take_pending() {
            batch.send(&self.peer);
        }
        self.reported()
    }

    /// The failure the last batch in transit left behind, as this call's
    /// — and as every later call's, once one has broken the stream.
    fn reported(&self) -> Result<()> {
        let unreported = self.ledger.take_unreported();
        unreported.or(self.ledger.broken()).map_or(Ok(()), Err)
    }

    /// Finish the stream: the items ran out, nothing went wrong.
    ///
    /// Queued items are flushed first, and that flush blocks for credit
    /// — unless the consumer has cancelled, in which case they are
    /// dropped and only the terminator goes out.
    ///
    /// Call it on every path, including the failing ones —
    /// [`end_with`](Self::end_with) is that path. A `Sink` dropped
    /// without either ends the stream as having failed, which is the
    /// only honest reading of a producer that vanished.
    pub fn end(self) -> Result<()> {
        self.terminate(ExceptionCode::None as i32, 0, None)
    }

    /// Finish the stream and say what went wrong.
    ///
    /// The status reaches the consumer as the error
    /// [`Receiver::recv`] yields, with its service-specific code and its
    /// message intact — the same failure the method that started the
    /// stream could have returned, arriving late because that method had
    /// already succeeded.
    ///
    /// A status that reports no exception ends the stream exactly as
    /// [`end`](Self::end) does.
    ///
    /// # Errors
    ///
    /// [`StatusCode::BadValue`] for a status that cannot be carried:
    /// `EX_TRANSACTION_FAILED` reports that the binder layer failed,
    /// which a call that arrived cannot claim, the two reply-header
    /// markers (`-127`, `-128`) mean nothing outside a reply, and
    /// `JustError` stands for a code this build could not read, which the
    /// consumer could not read either. The sink is
    /// consumed either way, so the stream still ends — with
    /// `EX_ILLEGAL_ARGUMENT`, saying the producer's own status was the
    /// thing that could not be sent.
    pub fn end_with(self, status: &Status) -> Result<()> {
        match status_fields(status) {
            Ok((exception, service_specific, message)) => {
                self.terminate(exception, service_specific, message.as_deref())
            }
            Err(e) => {
                let _ = self.terminate(
                    ExceptionCode::IllegalArgument as i32,
                    0,
                    Some(UNCARRIABLE_TERMINATOR),
                );
                Err(e)
            }
        }
    }

    fn terminate(
        mut self,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        // `Drop` runs next and must not send a second terminator.
        self.ended = true;
        // The terminator must not overtake a batch still on the pool. A
        // failure that batch left is taken here, so the flush below still
        // sends what is queued rather than returning it.
        self.ledger.wait_idle();
        let stale = self.ledger.take_unreported();
        let failed = match (self.flush(), stale) {
            (Err(e), _) => {
                // Nothing will send what that flush left queued.
                self.abandon_pending();
                Some(e)
            }
            (Ok(()), stale) => stale,
        };
        match failed {
            // Nobody is left to tell.
            Some(StatusCode::DeadObject) => Err(StatusCode::DeadObject),
            // A cancel drops the queued items and nothing else, so the
            // caller's own terminator stands.
            _ if self.credit.is_canceled() => {
                self.peer.on_end(exception, service_specific, message)
            }
            // The consumer is alive and blocked in `recv` whatever went
            // wrong here, so the terminator still goes out; the caller
            // hears about the flush, which is the earlier failure.
            Some(e) => {
                if let Err(end) = self.send_terminator(exception, service_specific, message) {
                    log::warn!("stream: the terminator could not be delivered: {end:?}");
                }
                Err(e)
            }
            // A clean flush leaves the caller's own terminator — unless
            // an earlier send lost items, which nothing else reports.
            None => self.send_terminator(exception, service_specific, message),
        }
    }

    /// Send the terminator the caller asked for, or the one that says
    /// items went missing when any did.
    fn send_terminator(
        &self,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        match truncated_terminator(exception, self.ledger.lost()) {
            Some(truncated) => {
                self.peer
                    .on_end(ExceptionCode::IllegalState as i32, 0, Some(&truncated))
            }
            None => self.peer.on_end(exception, service_specific, message),
        }
    }

    /// [`send`](Self::send) for a producer running as a task.
    ///
    /// Same contract, with the two waits moved off the executor thread:
    /// waiting for credit suspends the task, and the batch itself is
    /// sent from the blocking pool, the way every generated async proxy
    /// sends ([`Tokio`](crate::Tokio)).
    ///
    /// The item is encoded **when this is called**, not when the future
    /// is first polled, so the future does not borrow `item` and stays
    /// `Send` whatever `T` is. Dropping the future unpolled therefore
    /// leaves the item queued. Dropping it mid-wait is safe too: a credit
    /// not yet taken stays with the stream, and a batch already handed to
    /// the pool still goes out.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    #[must_use = "the item is queued, but a full batch is only sent when this is awaited"]
    pub fn send_async(
        &mut self,
        item: &T,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let encoded = self.usable().and_then(|()| self.encode(item));
        async move {
            if encoded? {
                self.flush_async().await?;
            }
            Ok(())
        }
    }

    /// [`flush`](Self::flush) for a producer running as a task.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    pub async fn flush_async(&mut self) -> Result<()> {
        self.usable()?;
        if self.count == 0 {
            return Ok(());
        }
        // As in `flush`.
        self.ledger.idle_async().await;
        self.reported()?;
        self.credit.wait_credit_async().await?;
        if let Some(batch) = self.take_pending() {
            let peer = self.peer.clone();
            // The outcome is the batch's to settle, not this future's:
            // dropping the future detaches the pool task, and a task the
            // runtime drops unrun never returns anything.
            let _ = on_pool(batch, move |batch| {
                batch.send(&peer);
                Ok(())
            })
            .await;
        }
        self.reported()
    }

    /// [`end`](Self::end) for a producer running as a task.
    ///
    /// Unlike [`send_async`](Self::send_async) and
    /// [`flush_async`](Self::flush_async), this is not a wait to abandon:
    /// the terminator becomes this call's only once it has been handed to
    /// the pool, and a future dropped before that leaves the stream to
    /// [`Drop`](Sink#impl-Drop-for-Sink), which ends it as failed rather
    /// than as done.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async). The
    /// terminator is still sent.
    #[cfg(feature = "tokio")]
    pub async fn end_async(self) -> Result<()> {
        self.terminate_async(ExceptionCode::None as i32, 0, None)
            .await
    }

    /// [`end_with`](Self::end_with) for a producer running as a task,
    /// with the same caveat about a dropped future, and the same panic,
    /// as [`end_async`](Self::end_async).
    #[cfg(feature = "tokio")]
    pub async fn end_with_async(self, status: &Status) -> Result<()> {
        match status_fields(status) {
            Ok((exception, service_specific, message)) => {
                self.terminate_async(exception, service_specific, message)
                    .await
            }
            Err(e) => {
                let _ = self
                    .terminate_async(
                        ExceptionCode::IllegalArgument as i32,
                        0,
                        Some(UNCARRIABLE_TERMINATOR.to_owned()),
                    )
                    .await;
                Err(e)
            }
        }
    }

    #[cfg(feature = "tokio")]
    async fn terminate_async(
        mut self,
        exception: i32,
        service_specific: i32,
        message: Option<String>,
    ) -> Result<()> {
        // As in `terminate`.
        self.ledger.idle_async().await;
        let stale = self.ledger.take_unreported();
        let failed = match (self.flush_async().await, stale) {
            (Err(e), _) => {
                // Nothing will send what that flush left queued.
                self.abandon_pending();
                Some(e)
            }
            (Ok(()), stale) => stale,
        };
        // Same rule as `terminate`: only a dead consumer leaves nobody to
        // send the terminator to.
        if let Some(StatusCode::DeadObject) = failed {
            self.ended = true;
            return Err(StatusCode::DeadObject);
        }
        let canceled = self.credit.is_canceled();
        // As in `terminate`: a cancel leaves the caller's own terminator,
        // and anything lost — by this flush or by an earlier send — is
        // reported here or nowhere.
        let (exception, service_specific, message) =
            match truncated_terminator(exception, self.ledger.lost()) {
                Some(truncated) if !canceled => {
                    (ExceptionCode::IllegalState as i32, 0, Some(truncated))
                }
                _ => (exception, service_specific, message),
            };
        let outcome = Arc::new(Mutex::new(None));
        let terminator = TerminatorInTransit {
            peer: self.peer.clone(),
            exception,
            service_specific,
            message,
            outcome: outcome.clone(),
            sent: false,
        };
        // Set only now: everything above suspends, and a future dropped
        // there has to leave `Drop` a stream still to terminate. From
        // here the terminator is the token's to send, whatever becomes
        // of this future or of the pool task.
        self.ended = true;
        let pooled = on_pool(terminator, |terminator| {
            terminator.send();
            Ok(())
        })
        .await;
        // The token's own answer where it has one: a terminator sent from
        // its `Drop`, because the pool never ran it, was still sent.
        let sent = outcome.lock().unwrap_or_else(|e| e.into_inner()).take();
        let ended = sent.unwrap_or(pooled);
        match failed {
            Some(e) if !canceled => Err(e),
            _ => ended,
        }
    }

    /// Whether the consumer has asked for no more items.
    ///
    /// The one way to tell a cancelled stream from an unusable transport
    /// when [`send`](Self::send) returns
    /// [`StatusCode::InvalidOperation`].
    pub fn is_canceled(&self) -> bool {
        self.credit.is_canceled()
    }

    /// Batches this producer may still send without waiting.
    pub fn credits(&self) -> u64 {
        self.credit.lock().available
    }

    /// Items queued for the next batch — accepted by
    /// [`send`](Self::send) and not yet sent.
    ///
    /// Zero right after [`flush`](Self::flush) and after any `send` that
    /// crossed the byte threshold. A non-zero reading while the consumer
    /// reports nothing arriving is the signature of a producer that
    /// should be flushing: the items are here, not lost and not in
    /// flight.
    pub fn pending(&self) -> usize {
        self.count as usize
    }

    /// Cut the pending batch back to `len` bytes by rebuilding it:
    /// `Parcel` has no truncate, and moving the write cursor back would
    /// not shorten the buffer.
    fn truncate_batch(&mut self, len: usize) -> Result<()> {
        let batch = std::mem::replace(&mut self.batch, Parcel::new_data_only());
        let mut bytes = batch.into_bytes()?;
        bytes.truncate(len);
        let mut rebuilt = Parcel::from_slice(&bytes);
        rebuilt.set_data_position(bytes.len());
        self.batch = rebuilt;
        Ok(())
    }
}

// Encoding items needs `Serialize`; handing the finished bytes over and
// terminating do not, and `Drop` has to be implemented for exactly the
// bounds the struct was declared with.
impl<T: ?Sized> Sink<T> {
    /// Take the pending batch out, leaving an empty one. The caller has
    /// taken a credit for it, which the batch now holds. `None` when the
    /// bytes could not be taken out; the ledger has that failure, and the
    /// credit is back.
    fn take_pending(&mut self) -> Option<BatchInTransit> {
        let batch = std::mem::replace(&mut self.batch, Parcel::new_data_only());
        let count = std::mem::replace(&mut self.count, 0);
        let mut transit = BatchInTransit::new(self.credit.clone(), self.ledger.clone(), count);
        match batch.into_bytes() {
            Ok(bytes) => {
                transit.bytes = bytes;
                Some(transit)
            }
            Err(e) => {
                transit.outcome = Outcome::Unsent(e);
                None
            }
        }
    }

    /// Give up on what is queued: nothing will send it, so it is lost.
    /// Taken out of the pending batch so that nothing counts it twice.
    fn abandon_pending(&mut self) {
        self.ledger.add_lost(std::mem::replace(&mut self.count, 0));
        self.batch = Parcel::new_data_only();
    }
}

/// A token and what to do with it, as the task a pool is handed. The
/// token sits in a slot rather than in the task so that [`carry`] can
/// take it back.
#[cfg(feature = "tokio")]
struct Carried<R, F> {
    slot: Arc<Mutex<Option<R>>>,
    run: F,
}

#[cfg(feature = "tokio")]
impl<R, F: FnOnce(R) -> Result<()>> Carried<R, F> {
    fn run(self) -> Result<()> {
        let token = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
        match token {
            Some(token) => (self.run)(token),
            // Taken back: the hand-off failed and the token has settled.
            None => Ok(()),
        }
    }
}

#[cfg(feature = "tokio")]
struct TakeBack<R> {
    slot: Arc<Mutex<Option<R>>>,
    armed: bool,
}

#[cfg(feature = "tokio")]
impl<R> Drop for TakeBack<R> {
    fn drop(&mut self) {
        if self.armed {
            let token = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
            // Dropped with the slot unlocked: settling makes binder calls.
            drop(token);
        }
    }
}

/// Give `hand_off` a task that runs `run` on `token`. A task dropped unrun
/// drops the token with it, which is how a token settles. The one way out
/// that skips even that is a hand-off that queues the task and then
/// unwinds — `spawn_blocking` does, when the OS refuses a thread — leaving
/// the task neither run nor dropped; the token is dropped here instead.
#[cfg(feature = "tokio")]
fn carry<R, F, H, O>(token: R, run: F, hand_off: H) -> O
where
    H: FnOnce(Carried<R, F>) -> O,
{
    let slot = Arc::new(Mutex::new(Some(token)));
    let mut guard = TakeBack {
        slot: slot.clone(),
        armed: true,
    };
    let handed = hand_off(Carried { slot, run });
    guard.armed = false;
    handed
}

/// Make one binder call from the blocking pool, with the token the call
/// is made for. Handed over when this is called, not when the future is
/// polled, so dropping the future detaches the call. `Err` says only that
/// the pool did not run it; what the call did is the token's to report.
/// Through [`Tokio`](crate::Tokio) rather than `spawn_blocking` directly,
/// because inside a transaction handler the call has to stay on the
/// current thread and that rule lives there.
#[cfg(feature = "tokio")]
fn on_pool<R, F>(token: R, run: F) -> crate::BoxFuture<'static, Result<()>>
where
    R: Send + 'static,
    F: FnOnce(R) -> Result<()> + Send + 'static,
{
    carry(token, run, |task| {
        <crate::Tokio as crate::BinderAsyncPool>::spawn(
            move || task.run(),
            |result| async move { result },
        )
    })
}

/// What an async consumer hands to [`on_pool`]: on the RPC stack a grant
/// waits for a free outgoing connection.
#[cfg(feature = "tokio")]
fn send_grant(grant: GrantToken) -> Result<()> {
    grant.send();
    Ok(())
}

impl<T: ?Sized> Drop for Sink<T> {
    /// Deliver what is queued and end the stream.
    ///
    /// A producer that goes away without [`end`](Sink::end) leaves the
    /// consumer blocked in [`Receiver::recv`] for a terminator that no
    /// longer has a sender, and the consumer cannot recover on its own:
    /// the producer's process is still running, so no death link fires.
    /// This is the fallback for that, not a substitute for `end` — the
    /// stream is reported as having failed, since a producer that
    /// vanished mid-stream has no way to say it was done.
    ///
    /// The flush here does not wait for credit. Blocking in a `Drop`
    /// would hold the unwinding thread for as long as the consumer takes
    /// to grant, so items that cannot leave now are lost and the
    /// terminator's message says how many. It does wait for a batch that
    /// a dropped `*_async` future left on the blocking pool, which is
    /// bounded by that one send rather than by the consumer, and without
    /// which the terminator would overtake the batch.
    fn drop(&mut self) {
        // Unlinked whether or not the stream was ended: dropping the
        // recipient leaves a dead `Weak` in the sink's recipient list,
        // which never shrinks on its own.
        let death = self.death.take();
        unlink_death(&self.sink, &death);
        if self.ended {
            return;
        }
        // The terminator must not overtake a batch still on the pool.
        self.ledger.wait_idle();
        if self.count > 0 {
            if self.ledger.broken().is_none() && self.credit.try_credit() {
                if let Some(batch) = self.take_pending() {
                    batch.send(&self.peer);
                }
            } else {
                self.abandon_pending();
            }
        }
        if let Some(e) = self.ledger.take_unreported() {
            log::warn!("stream: a batch could not be delivered: {e:?}");
        }
        // Everything lost since the stream began: nothing after this
        // terminator can report it.
        let lost = self.ledger.lost();
        let message = format!(
            "the stream's producer dropped its sink without ending the stream; \
             {lost} queued item(s) were not sent"
        );
        if let Err(e) = self
            .peer
            .on_end(ExceptionCode::IllegalState as i32, 0, Some(&message))
        {
            log::warn!("stream: the terminator could not be delivered: {e:?}");
        }
    }
}

impl<I: FromIBinder + ?Sized> Peer<I> {
    /// The RPC proxy behind an unstamped peer, for a hand-built call.
    #[cfg(feature = "rpc")]
    fn rpc_proxy(binder: &SIBinder) -> Result<&crate::rpc::RpcProxy> {
        (**binder)
            .as_any()
            .downcast_ref::<crate::rpc::RpcProxy>()
            .ok_or(StatusCode::BadType)
    }
}

impl Peer<dyn IStreamSink> {
    /// Whether calls on this peer cross an RPC session, where a failed
    /// send means something different from a failed kernel transaction.
    fn over_rpc(&self) -> bool {
        match self {
            #[cfg(feature = "rpc")]
            Peer::Typed(sink) => (*sink.as_binder())
                .as_any()
                .downcast_ref::<crate::rpc::RpcProxy>()
                .is_some(),
            #[cfg(not(feature = "rpc"))]
            Peer::Typed(_) => false,
            #[cfg(feature = "rpc")]
            Peer::Unstamped(_) => true,
        }
    }

    fn on_start(&self, source: &SIBinder, credits: i32) -> Result<()> {
        match self {
            Peer::Typed(sink) => sink.r#onStart(source, credits).map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data = proxy.build_request(<BpStreamSink as crate::Proxy>::descriptor())?;
                data.write(source)?;
                data.write(&credits)?;
                proxy
                    .transact(sink_transactions::r#onStart, &data, ONEWAY_FLAGS)
                    .map(|_| ())
            }
        }
    }

    fn on_batch(&self, items: &[u8], count: i32) -> Result<()> {
        match self {
            Peer::Typed(sink) => sink.r#onBatch(items, count).map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data = proxy.build_request(<BpStreamSink as crate::Proxy>::descriptor())?;
                data.write(items)?;
                data.write(&count)?;
                proxy
                    .transact(sink_transactions::r#onBatch, &data, ONEWAY_FLAGS)
                    .map(|_| ())
            }
        }
    }

    fn on_end(&self, exception: i32, service_specific: i32, message: Option<&str>) -> Result<()> {
        match self {
            Peer::Typed(sink) => sink
                .r#onEnd(exception, service_specific, message)
                .map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data = proxy.build_request(<BpStreamSink as crate::Proxy>::descriptor())?;
                data.write(&exception)?;
                data.write(&service_specific)?;
                data.write(&message)?;
                proxy
                    .transact(sink_transactions::r#onEnd, &data, ONEWAY_FLAGS)
                    .map(|_| ())
            }
        }
    }
}

/// What the generated proxy puts on a `oneway` call
/// (`rsbinder-aidl/src/generator.rs`); a hand-built request has to match
/// it or the two paths would differ on the wire.
#[cfg(feature = "rpc")]
const ONEWAY_FLAGS: crate::TransactionFlags =
    crate::FLAG_ONEWAY | crate::FLAG_CLEAR_BUF | crate::FLAG_PRIVATE_LOCAL;

// ---------------------------------------------------------------------
// Status on the `onEnd` wire
// ---------------------------------------------------------------------

/// What the consumer is told when [`Sink::end_with`] was handed a status
/// the wire cannot carry. The sink is consumed by then, so the stream ends
/// on this rather than on the producer's own status.
const UNCARRIABLE_TERMINATOR: &str =
    "the stream's producer ended with a status that cannot be carried";

/// The terminator for a stream whose last flush failed: a clean `EX_NONE`
/// over lost items would read exactly like a stream that ran out, so it is
/// replaced — but only when the producer has no failure of its own to
/// report, which is the more specific answer.
fn truncated_terminator(exception: i32, lost: i32) -> Option<String> {
    if lost <= 0 || exception != ExceptionCode::None as i32 {
        return None;
    }
    Some(format!(
        "the stream's producer could not deliver its last batch; \
         {lost} queued item(s) were not sent"
    ))
}

/// Split a [`Status`] into the three `onEnd` arguments.
fn status_fields(status: &Status) -> Result<(i32, i32, Option<String>)> {
    let exception = status.exception_code();
    match exception {
        // What `exception_from_i32` would refuse on the other side.
        ExceptionCode::TransactionFailed
        | ExceptionCode::HasNotedAppOpsReplyHeader
        | ExceptionCode::HasReplyHeader
        | ExceptionCode::JustError => {
            log::error!("Sink::end: {exception} cannot be carried as a stream terminator");
            Err(StatusCode::BadValue)
        }
        ExceptionCode::ServiceSpecific => Ok((
            exception as i32,
            status.service_specific_error(),
            status.message().map(str::to_owned),
        )),
        _ => Ok((exception as i32, 0, status.message().map(str::to_owned))),
    }
}

/// Rebuild a [`Status`] from the three `onEnd` arguments, reading
/// `service_specific` only for `EX_SERVICE_SPECIFIC` — the one exception
/// AOSP's `Status::writeToParcel` writes that field for.
fn status_from_fields(
    exception: i32,
    service_specific: i32,
    message: Option<&str>,
) -> Result<Status> {
    let code = exception_from_i32(exception)?;
    Ok(match (code, message) {
        (ExceptionCode::ServiceSpecific, _) => {
            Status::new_service_specific_error(service_specific, message.map(str::to_owned))
        }
        (code, None) => Status::from(code),
        (code, Some(message)) => Status::from((code, message)),
    })
}

fn exception_from_i32(exception: i32) -> Result<ExceptionCode> {
    Ok(match exception {
        0 => ExceptionCode::None,
        -1 => ExceptionCode::Security,
        -2 => ExceptionCode::BadParcelable,
        -3 => ExceptionCode::IllegalArgument,
        -4 => ExceptionCode::NullPointer,
        -5 => ExceptionCode::IllegalState,
        -6 => ExceptionCode::NetworkMainThread,
        -7 => ExceptionCode::UnsupportedOperation,
        -8 => ExceptionCode::ServiceSpecific,
        -9 => ExceptionCode::Parcelable,
        // -127 and -128 say "a blob precedes the real code", which a
        // reply has and this call does not; -129 says the binder layer
        // failed, which a call that arrived cannot report. Anything else
        // is a peer speaking a code this build does not know.
        other => {
            log::error!("stream: {other} is not a valid `onEnd` exception code");
            return Err(StatusCode::BadValue);
        }
    })
}

// ---------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------

/// What the sink object receives and what the consumer has asked for,
/// shared between the binder threads that deliver the producer's calls
/// and the one thread or task that consumes.
#[derive(Default)]
struct StreamState {
    /// Undecoded batches: bytes and the item count the producer stated.
    /// Decoding happens on the consumer's thread so a malformed batch
    /// surfaces as its error rather than being dropped on a binder
    /// worker.
    batches: VecDeque<(Vec<u8>, i32)>,
    /// Set once — by `onEnd`, by the producer's death, or by a grant that
    /// could not be sent with nothing left to prompt another. `None`
    /// while the stream runs.
    end: Option<Status>,
    /// Where to grant and cancel; `None` until `onStart` arrives.
    source: Option<Arc<Peer<dyn IStreamSource>>>,
    /// The same binder, kept for the unlink in `Receiver`'s `Drop`.
    source_binder: Option<SIBinder>,
    /// Holds the death link on the source; the binder keeps only a
    /// `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    /// The consumer wants no more. An `onStart` that arrives after this
    /// is answered with a cancel on the spot.
    canceled: bool,
    /// The receiver is gone, so nothing will drain a batch again.
    closed: bool,
    /// The opening window the producer stated in `onStart`.
    declared: u32,
    /// Batches accepted into the queue.
    received: u64,
    /// Batches the consumer has taken out of it. A grant is this number:
    /// the running total `IStreamSource.request` carries. Under this lock
    /// with the two below because the consumer's wait reads them.
    drained: u64,
    /// The highest total a grant has been tried with. Whether a failed
    /// `oneway` send arrived cannot always be told, so a total that was
    /// tried counts as one the producer may have — and being a total, it
    /// counts once however often it is tried.
    announced: u64,
    /// The highest total a grant was sent with.
    granted: u64,
    /// The last grant failed. Retrying at once would spin, so the next
    /// try waits for a batch or for the consumer's next call.
    grant_failed: bool,
}

impl StreamState {
    /// Batches drained that no grant has been sent for.
    fn owed(&self) -> u64 {
        self.drained - self.granted
    }

    /// The most batches the producer can have credit for. One beyond it
    /// is a producer sending without credit.
    fn issued(&self) -> u64 {
        self.declared as u64 + self.announced
    }

    /// The producer may have no credit left to send with: it has used
    /// everything it is certain to have been given.
    fn producer_may_be_starved(&self) -> bool {
        self.received >= self.declared as u64 + self.granted
    }

    /// Nothing queued to drain and a producer that may be unable to
    /// send: a grant that fails now has nothing to prompt a retry.
    fn still_last_chance(&self) -> bool {
        self.batches.is_empty() && self.producer_may_be_starved()
    }
}

struct Stream {
    state: Mutex<StreamState>,
    arrived: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
    /// The widest opening window this consumer takes from a producer.
    max_opening: u32,
}

impl Stream {
    fn new(max_opening: u32) -> Self {
        Stream {
            state: Mutex::default(),
            arrived: Condvar::new(),
            #[cfg(feature = "tokio")]
            notify: tokio::sync::Notify::new(),
            max_opening,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, StreamState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn wake(&self) {
        self.arrived.notify_all();
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
    }

    /// End the stream with `status` unless something already has.
    fn end_with(&self, status: Status) {
        {
            let mut state = self.lock();
            if state.end.is_some() {
                return;
            }
            state.end = Some(status);
        }
        self.wake();
    }

    /// End a stream the consumer's side gave up on. Unlike a terminator
    /// or a death, the producer does not know: it is alive, will get no
    /// more credit, and is released only by the cancel this returns.
    /// `overriding` replaces an end already recorded — for a failure the
    /// consumer is being handed, which outranks a terminator that arrived
    /// first and would have [`Receiver::end_status`] report success.
    fn fail(&self, status: Status, overriding: bool) -> CancelDue {
        let source = {
            let mut state = self.lock();
            if overriding || state.end.is_none() {
                state.end = Some(status);
            }
            // So an `onStart` still on its way is answered on arrival.
            state.canceled = true;
            state.source.clone()
        };
        self.wake();
        CancelDue(source)
    }
}

/// The cancel a failed stream owes its producer. Sent on drop, so no path
/// can end the stream and forget it; a value rather than a call so an
/// async caller can move it off the executor thread first.
#[must_use = "dropping it sends the cancel; move it to where that may block"]
struct CancelDue(Option<Arc<Peer<dyn IStreamSource>>>);

impl CancelDue {
    fn send(self) {}
}

impl Drop for CancelDue {
    fn drop(&mut self) {
        if let Some(source) = self.0.take() {
            let _ = source.cancel();
        }
    }
}

/// One grant: the running total to announce. It holds nothing that has to
/// be given back — what is owed is `drained - granted`, and a grant that
/// is refused, or carried by a pool task that never ran, leaves `granted`
/// where it was, so the next grant announces the same total or a higher
/// one. The producer counts a total once however often it hears it.
struct GrantToken {
    stream: Arc<Stream>,
    source: Arc<Peer<dyn IStreamSource>>,
    total: u64,
    /// The consumer is about to block with the producer possibly out of
    /// credit, so nothing would prompt a retry: a failure ends the stream.
    last_chance: bool,
}

impl GrantToken {
    fn send(self) {
        match self.source.request(self.total as i64) {
            Ok(()) => {
                let mut state = self.stream.lock();
                state.granted = state.granted.max(self.total);
                state.grant_failed = false;
            }
            // Asked again now: a batch that landed since the token was made
            // is drained next, and draining it tries the grant again.
            Err(e) if self.last_chance && self.stream.lock().still_last_chance() => {
                log::warn!("stream: granting up to {} failed: {e:?}", self.total);
                // Both ends would otherwise wait on each other.
                self.stream.fail(Status::from(e), false).send();
            }
            Err(e) => {
                log::warn!(
                    "stream: granting up to {} failed, will retry: {e:?}",
                    self.total
                );
                // What keeps the retry from happening at once.
                self.stream.lock().grant_failed = true;
            }
        }
    }
}

/// Watches the producer's source and ends the stream the way a terminator
/// does, with [`StatusCode::DeadObject`], so a consumer blocked in
/// [`Receiver::recv`] learns that no batch is coming without a case of its
/// own. `Weak`, because the stream state owns this recipient.
struct SourceDeath(std::sync::Weak<Stream>);

impl crate::DeathRecipient for SourceDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        if let Some(stream) = self.0.upgrade() {
            stream.end_with(Status::from(StatusCode::DeadObject));
        }
    }
}

/// The binder the consumer hands to the producer.
struct SinkObject(Arc<Stream>);

impl Interface for SinkObject {}

impl IStreamSink for SinkObject {
    fn r#onStart(&self, source: &SIBinder, credits: i32) -> BinderResult<()> {
        let resolve = || {
            resolve_peer::<dyn IStreamSource>(
                source,
                <BpStreamSource as crate::Proxy>::descriptor(),
                "IStreamSink.onStart",
            )
        };
        // Looked at before anything below can refuse: a second `onStart`
        // is somebody else's mistake, and must not end a stream that is
        // running, whatever it carries. The locked check further down
        // settles a race.
        let second = self
            .0
            .lock()
            .source_binder
            .as_ref()
            .map(|kept| *kept != SIBinder::downgrade(source));
        if let Some(other_producer) = second {
            log::warn!("stream: ignoring a second onStart");
            if other_producer {
                if let Ok(peer) = resolve() {
                    let _ = peer.cancel();
                }
            }
            return Ok(());
        }
        let peer = match resolve() {
            Ok(peer) => Arc::new(peer),
            Err(code) => {
                // Without a usable source there is no way to grant, so
                // the stream would stall after the opening window. Say
                // so now; the call is `oneway`, so the consumer is the
                // only one who can be told.
                self.0.fail(Status::from(code), false).send();
                return Ok(());
            }
        };
        let window = u32::try_from(credits).ok().filter(|n| *n > 0);
        let Some(window) = window.filter(|n| *n <= self.0.max_opening) else {
            let message = format!(
                "the producer opens with {credits} batches of credit; \
                 this consumer accepts 1 to {}",
                self.0.max_opening
            );
            log::error!("stream: {message}");
            // The producer has whatever it stated and would spend it on
            // batches this stream discards, then park.
            let _ = peer.cancel();
            self.0
                .fail(
                    Status::from((ExceptionCode::IllegalArgument, message.as_str())),
                    false,
                )
                .send();
            return Ok(());
        };
        // Linked before taking the lock: no binder call under it.
        let death = match watch_death(source, SourceDeath(Arc::downgrade(&self.0))) {
            Ok(death) => death,
            Err(code) => {
                // Unwatched, a producer that dies leaves the consumer
                // blocked for a batch that cannot come. The producer is
                // told too: without this it spends its opening window on
                // batches this stream now discards and then parks on
                // credit no drain will ever prompt. `fail` cancels the
                // source the stream kept, which this one never became.
                let _ = peer.cancel();
                self.0.fail(Status::from(code), false).send();
                return Ok(());
            }
        };
        let cancel_now;
        let mut kept = false;
        {
            let mut state = self.0.lock();
            if let Some(kept_source) = &state.source_binder {
                log::warn!("stream: ignoring a second onStart");
                // A second producer that is not told parks on credit this
                // stream will never grant, since every grant goes to the
                // source kept above. The same producer sending `onStart`
                // twice is the stream itself and must not be cancelled —
                // compared through `WIBinder`, which is the identity that
                // survives a proxy's resurrection.
                cancel_now = *kept_source != SIBinder::downgrade(source);
            } else {
                kept = !state.closed;
                if kept {
                    state.source = Some(peer.clone());
                    state.source_binder = Some(source.clone());
                    state.death = death.clone();
                    state.declared = window;
                }
                cancel_now = state.canceled;
            }
        }
        if !kept {
            // Nothing owns this link now, and dropping the recipient
            // would only make it inert.
            unlink_death(source, &death);
        }
        if cancel_now {
            // The consumer gave up before the producer got this far.
            let _ = peer.cancel();
        }
        Ok(())
    }

    fn r#onBatch(&self, items: &[u8], count: i32) -> BinderResult<()> {
        if count < 0 {
            let status = Status::from((
                ExceptionCode::IllegalArgument,
                "IStreamSink.onBatch needs a non-negative item count",
            ));
            // The call is `oneway`, so this return value is logged and
            // dropped; ending the stream is what reaches the consumer,
            // which would otherwise read a peer that skipped a batch as
            // one that sent every item. Same treatment as a batch whose
            // bytes and `count` disagree (`Receiver::batch_failed`).
            self.0.fail(status.clone(), false).send();
            return Err(status);
        }
        {
            let mut state = self.0.lock();
            // After the terminator nothing is part of the stream, and
            // after the receiver is gone nothing drains. Dropping is
            // quieter than growing a queue nobody reads.
            if state.end.is_some() || state.closed {
                return Ok(());
            }
            if state.received < state.issued() {
                state.received += 1;
                // Something arrived, so a grant is worth trying again.
                state.grant_failed = false;
                state.batches.push_back((items.to_vec(), count));
                drop(state);
                self.0.wake();
                return Ok(());
            }
        }
        // Credit is what bounds this queue. A producer sending without it
        // — including one that sends before `onStart` — is not paced by
        // anything, and queueing for it would grow without limit.
        let status = Status::from((
            ExceptionCode::IllegalState,
            "the producer sent a batch it had no credit for",
        ));
        self.0.fail(status.clone(), false).send();
        Err(status)
    }

    fn r#onEnd(
        &self,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> BinderResult<()> {
        let status = match status_from_fields(exception, service_specific, message) {
            Ok(status) => status,
            // The terminator is unreadable, so the stream still has to
            // stop; report the decode failure as the reason it did.
            Err(code) => Status::from(code),
        };
        self.0.end_with(status);
        Ok(())
    }
}

/// What the queue holds right now.
enum Queued {
    Batch((Vec<u8>, i32)),
    /// Nothing queued and the stream is over.
    End,
    /// Nothing queued and the stream is still running.
    Nothing,
}

/// The consumer's end of a stream: items come out in the order the
/// producer sent them.
///
/// Made by [`Receiver::new`], which also produces the sink binder to pass
/// to the service. That is the whole setup: the producer introduces
/// itself over the sink, and from then on the receiver grants credit as
/// it drains and watches the producer for death.
///
/// Consume with [`recv`](Self::recv), or by iterating: `Receiver`
/// implements [`Iterator`] over `BinderResult<T>`, ending when the
/// producer's terminator arrives.
pub struct Receiver<T> {
    stream: Arc<Stream>,
    sink_binder: SIBinder,
    window: u32,
    /// Items decoded from the batch being drained.
    decoded: VecDeque<T>,
    /// Set once the terminator or a decode failure has been reported, so
    /// the error is yielded once and the stream then reads as finished.
    finished: bool,
    /// This call has not yet retried a grant that failed; it gets one try.
    retry_armed: bool,
}

impl<T: Deserialize> Receiver<T> {
    /// A receiver and the sink binder to hand to the producer.
    ///
    /// Pass the binder to the method that starts the stream — declared in
    /// `.aidl` as `rsbinder.stream.IStreamSink` or as a bare `IBinder`.
    pub fn new() -> (Self, SIBinder) {
        Self::with_limits(DEFAULT_CREDIT_WINDOW, DEFAULT_CREDIT_WINDOW)
    }

    /// [`new`](Self::new) with the grant window set explicitly.
    ///
    /// This is how many drained batches the receiver lets go unpaid
    /// before it grants: a grant leaves once half a window is owed — or
    /// the producer's whole opening window, if that is less — and
    /// whatever is owed leaves unconditionally once the receiver has
    /// nothing left to drain. A larger window means fewer grant
    /// transactions and a longer wait before the producer sees one.
    ///
    /// It does not set how many batches the producer may have in flight.
    /// That is the producer's opening window — `initial_credits` in
    /// [`Sink::with_limits`] — and it stays the ceiling, because a grant
    /// only ever pays for a batch already drained. How wide a window this
    /// receiver takes is [`with_limits`](Self::with_limits)'s second
    /// argument, here [`DEFAULT_CREDIT_WINDOW`].
    pub fn with_credit_window(window: u32) -> (Self, SIBinder) {
        Self::with_limits(window, DEFAULT_CREDIT_WINDOW)
    }

    /// [`with_credit_window`](Self::with_credit_window), and the widest
    /// opening window this receiver accepts from a producer.
    ///
    /// The producer states its opening window when it introduces itself,
    /// and the receiver holds it to that: a batch sent without credit
    /// ends the stream with `EX_ILLEGAL_STATE` instead of being queued.
    /// That is what bounds the memory a producer can make this process
    /// hold, at `max_opening` batches: a grant only pays for a batch
    /// already drained, and being a running total it is counted once
    /// however often it has to be sent. It is why the bound is the
    /// consumer's to set: a
    /// service taking an upload is the consumer, and its producer is a
    /// client it has no reason to trust. A producer that opens wider than
    /// `max_opening` is refused at the start, with
    /// `EX_ILLEGAL_ARGUMENT`, rather than part-way through.
    ///
    /// The default is [`DEFAULT_CREDIT_WINDOW`], the producer's own
    /// default, whose in-flight bytes that constant's documentation
    /// accounts for. Raise it together with `initial_credits` on the
    /// producer's side; zero is taken as one.
    pub fn with_limits(window: u32, max_opening: u32) -> (Self, SIBinder) {
        let stream = Arc::new(Stream::new(max_opening.max(1)));
        let sink_binder = BnStreamSink::new_binder(SinkObject(stream.clone())).as_binder();
        let receiver = Receiver {
            stream,
            sink_binder: sink_binder.clone(),
            window: window.max(1),
            decoded: VecDeque::new(),
            finished: false,
            retry_armed: false,
        };
        (receiver, sink_binder)
    }

    /// The sink binder again, for a caller that did not keep the one
    /// [`new`](Self::new) returned.
    pub fn sink_binder(&self) -> SIBinder {
        self.sink_binder.clone()
    }

    /// The next item, blocking until one arrives.
    ///
    /// `None` once the producer's terminator has arrived and every item
    /// before it has been returned. An error ends the stream too: it is
    /// yielded once, and the call after it reports `None`.
    ///
    /// The status the producer ended with — including a clean one — is
    /// available from [`end_status`](Self::end_status) afterwards.
    ///
    /// A grant that cannot be sent ends the stream, with the error the
    /// send failed with, only when nothing else could move it: this call
    /// is about to block, and the producer has used all the credit it is
    /// known to have. A producer with credit left sends again, and the
    /// grant is tried again when that batch is drained.
    pub fn recv(&mut self) -> Option<BinderResult<T>> {
        self.retry_armed = true;
        loop {
            if let Some(done) = self.advance(true) {
                return done;
            }
            // Nothing queued. Re-checked under the lock, so a batch that
            // landed since `advance` looked is not slept through.
            let mut state = self.stream.lock();
            while self.must_wait(&state) {
                state = self
                    .stream
                    .arrived
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    /// The next item if one is already here, without waiting for one.
    ///
    /// `Ok(None)` means nothing has arrived yet and the stream is still
    /// running; the stream being over reads as `Ok(None)` too, which
    /// [`is_finished`](Self::is_finished) tells apart.
    ///
    /// It never waits for an item, but it is not free: with credit owed
    /// and nothing left to drain it grants before answering, because a
    /// producer out of credit sends nothing that would prompt a later
    /// grant. That grant is one `oneway` call, and on the RPC stack it
    /// waits for a free outgoing connection on the session — so an event
    /// loop polling this one can be held up for as long as the session's
    /// outgoing connections are busy.
    ///
    /// A grant that fails here does not end the stream, as it may in
    /// [`recv`](Self::recv): this call returns, and the next one tries
    /// the grant again.
    pub fn try_recv(&mut self) -> BinderResult<Option<T>> {
        self.retry_armed = true;
        match self.advance(false) {
            Some(done) => done.transpose(),
            None => Ok(None),
        }
    }

    /// [`recv`](Self::recv) with a bound on how long it waits.
    ///
    /// `Ok(None)` when `timeout` elapses with the stream still running.
    /// A timeout leaves the stream usable — call again.
    ///
    /// `timeout` bounds the wait for an item only. The grant
    /// [`try_recv`](Self::try_recv) describes is sent before that wait
    /// starts and is not counted against it, so this can return later
    /// than `timeout` by however long a grant takes. One that fails is
    /// tried again by the next call, not within this one.
    ///
    /// A `timeout` too long for an `Instant` to express — `Duration::MAX`
    /// — waits without a bound, and is [`recv`](Self::recv) in every
    /// respect, the last-chance grant included.
    pub fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        // A `timeout` no `Instant` can express waits without a bound
        // rather than panicking in `Add` — and is then a `recv`, with
        // nothing coming after it to retry a grant.
        let deadline = Instant::now().checked_add(timeout);
        self.retry_armed = true;
        loop {
            if let Some(done) = self.advance(deadline.is_none()) {
                return done.transpose();
            }
            // Here as well as in the wait below, so that `timeout` bounds
            // this call whatever sends it round the loop.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            let mut state = self.stream.lock();
            while self.must_wait(&state) {
                let Some(deadline) = deadline else {
                    state = self
                        .stream
                        .arrived
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                    continue;
                };
                let left = match deadline.checked_duration_since(Instant::now()) {
                    Some(left) if !left.is_zero() => left,
                    _ => return Ok(None),
                };
                let (next, _) = self
                    .stream
                    .arrived
                    .wait_timeout(state, left)
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
            }
        }
    }

    /// [`recv`](Self::recv) for an async consumer.
    ///
    /// Same result as `recv`, waiting on a notification instead of
    /// parking the thread. No `Stream` trait is implemented and none is
    /// in the public signature: that would bind rsbinder's API to a
    /// `futures-core` major version. Adapt it where you need one.
    ///
    /// Granting credit is a `oneway` call, and it still returns before
    /// the producer has run, but it is not free: on the RPC stack it
    /// waits for a free outgoing connection on the session. It therefore
    /// goes to the blocking pool rather than holding the executor thread,
    /// the way [`Sink::send_async`] sends. Cancel-safe: a future dropped
    /// mid-wait loses no item and no credit — a grant already handed to
    /// the pool still goes out, and the next call may send the same
    /// running total again, which the producer counts once.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    /// Nothing grants before a batch has been drained, so a consumer
    /// polled outside one fails on a later call rather than the first.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        self.retry_armed = true;
        loop {
            // Created before looking: a `Notified` receives
            // `notify_waiters` from the moment it exists, so a batch
            // landing between the look and the await is not missed.
            // Through a clone of the handle so the waiter does not borrow
            // `self` across `advance_async`.
            let stream = self.stream.clone();
            let notified = stream.notify.notified();
            if let Some(done) = self.advance_async().await {
                return done;
            }
            notified.await;
        }
    }

    /// Whether the stream is over — the terminator arrived and every
    /// item before it has been handed out.
    pub fn is_finished(&self) -> bool {
        self.finished
    }

    /// How the producer ended the stream, once it has.
    ///
    /// `None` while the stream is still running. After it ends this is
    /// the status the producer passed to [`Sink::end`], including
    /// `Status::ok()` for a stream that simply ran out.
    pub fn end_status(&self) -> Option<Status> {
        self.stream.lock().end.clone()
    }

    /// Tell the producer to stop.
    ///
    /// Releases a producer parked waiting for credit, and its
    /// [`Sink::send`] then reports [`StatusCode::InvalidOperation`].
    /// Items already in flight may still arrive, and the producer may
    /// still send a terminator. A cancel made before the producer has
    /// introduced itself is held and sent the moment it does.
    ///
    /// Sent automatically when a `Receiver` is dropped, so a consumer
    /// that walks away does not leave the producer waiting.
    pub fn cancel(&self) -> Result<()> {
        let source = {
            let mut state = self.stream.lock();
            state.canceled = true;
            state.source.clone()
        };
        match source {
            Some(source) => source.cancel(),
            None => Ok(()),
        }
    }

    /// Hand out an item, or decode the next batch, or report the end.
    /// `None` means nothing is queued and the stream is still running, so
    /// the caller waits — and whatever credit was owed has been granted
    /// first, because a producer out of credit sends nothing that would
    /// prompt a later grant. `blocking` says the caller will then wait
    /// without a bound, which is what can make that grant the last chance.
    fn advance(&mut self, blocking: bool) -> Option<Option<BinderResult<T>>> {
        loop {
            if let Some(answer) = self.ready() {
                return Some(answer);
            }
            match self.poll_queue() {
                Queued::Batch(batch) => match self.take_batch(batch) {
                    Ok(()) => {
                        if let Some(grant) = self.grant_due(false, false) {
                            grant.send();
                        }
                    }
                    Err(e) => {
                        let (answer, cancel) = self.batch_failed(e);
                        cancel.send();
                        return Some(answer);
                    }
                },
                Queued::End => return Some(self.finish()),
                Queued::Nothing => {
                    self.grant_due(true, blocking)?.send();
                    // A failed grant may have ended the stream; look again
                    // rather than wait for a batch that cannot come.
                    let ended = self.stream.lock().end.is_some();
                    if !ended {
                        return None;
                    }
                }
            }
        }
    }

    /// [`advance`](Self::advance) with the grants moved off the executor
    /// thread.
    #[cfg(feature = "tokio")]
    async fn advance_async(&mut self) -> Option<Option<BinderResult<T>>> {
        loop {
            if let Some(answer) = self.ready() {
                return Some(answer);
            }
            match self.poll_queue() {
                Queued::Batch(batch) => match self.take_batch(batch) {
                    Ok(()) => {
                        if let Some(grant) = self.grant_due(false, false) {
                            // Still owed if the pool did not run it, and
                            // the idle grant below is where that ends.
                            let _ = on_pool(grant, send_grant).await;
                        }
                    }
                    Err(e) => {
                        let (answer, cancel) = self.batch_failed(e);
                        // Not awaited: the error is this call's to yield,
                        // and a future dropped at an await would lose it.
                        drop(on_pool(cancel, |cancel| {
                            cancel.send();
                            Ok(())
                        }));
                        return Some(answer);
                    }
                },
                Queued::End => return Some(self.finish()),
                Queued::Nothing => {
                    if let Err(e) = on_pool(self.grant_due(true, true)?, send_grant).await {
                        // The pool is gone, so what is owed can never be
                        // sent from here, and nothing would wake a
                        // consumer that went on to wait. The cancel goes
                        // from this thread for the same reason.
                        self.stream.fail(Status::from(e), false).send();
                    }
                    let ended = self.stream.lock().end.is_some();
                    if !ended {
                        return None;
                    }
                }
            }
        }
    }

    /// The answer that needs no queue: an item already decoded, or the
    /// end of a stream whose outcome has been reported.
    fn ready(&mut self) -> Option<Option<BinderResult<T>>> {
        if let Some(item) = self.decoded.pop_front() {
            return Some(Some(Ok(item)));
        }
        self.finished.then_some(None)
    }

    /// Report a batch that would not decode, and stop the producer:
    /// nothing was counted as owed for it, so no grant follows and the
    /// producer would park on credit that cannot come.
    fn batch_failed(&mut self, e: StatusCode) -> (Option<BinderResult<T>>, CancelDue) {
        self.finished = true;
        // Overriding, not deferred to whatever ended the stream first:
        // the producer's terminator may already be sitting there, and the
        // consumer is being handed this error right now.
        let cancel = self.stream.fail(Status::from(e), true);
        (Some(Err(e.into())), cancel)
    }

    /// Whether a consumer with nothing to hand out has to park. An empty
    /// queue and no end is not enough: credit that came back from a grant
    /// nobody sent is one [`advance`](Self::advance) can send now, and the
    /// producer may be parked waiting for exactly that. Credit that came
    /// back from a grant that *failed* is not: that waits for a batch.
    fn must_wait(&self, state: &StreamState) -> bool {
        if !state.batches.is_empty() || state.end.is_some() {
            return false;
        }
        state.owed() == 0 || state.grant_failed
    }

    fn poll_queue(&self) -> Queued {
        let mut state = self.stream.lock();
        match state.batches.pop_front() {
            Some(batch) => Queued::Batch(batch),
            None if state.end.is_some() => Queued::End,
            None => Queued::Nothing,
        }
    }

    /// Decode a batch and count it as owed; the caller grants.
    fn take_batch(&mut self, (bytes, count): (Vec<u8>, i32)) -> Result<()> {
        match self.decode_batch(&bytes, count) {
            Ok(()) => {
                self.stream.lock().drained += 1;
                Ok(())
            }
            Err(e) => {
                // Whatever decoded before the failure came out of a batch
                // that turned out to be malformed; handing those items
                // out after reporting the error would contradict it.
                self.decoded.clear();
                Err(e)
            }
        }
    }

    fn decode_batch(&mut self, bytes: &[u8], count: i32) -> Result<()> {
        let mut parcel = Parcel::from_slice(bytes);
        for decoded in 0..count {
            // Checked rather than trusted: `count` comes off the wire,
            // and without this a peer claiming `i32::MAX` items in an
            // empty batch would spin here. It also rules out an item
            // type that encodes to nothing, which no count could
            // describe.
            if parcel.data_avail() == 0 {
                log::error!("stream: batch claims {count} items, bytes ran out after {decoded}");
                return Err(StatusCode::NotEnoughData);
            }
            self.decoded.push_back(parcel.read::<T>()?);
        }
        if parcel.data_avail() != 0 {
            // The producer said `count` items and sent more bytes than
            // that many decode to. Reading on would be guessing at where
            // the next item starts.
            log::error!(
                "stream: {} bytes left after {count} items",
                parcel.data_avail()
            );
            return Err(StatusCode::BadValue);
        }
        Ok(())
    }

    /// The grant that is due now. With more batches already queued (`all`
    /// false) none is due until half a window is owed, or the producer's
    /// whole opening window if that is less; with nothing queued whatever
    /// is owed is due. `blocking` says the caller is about to wait
    /// without a bound: if the producer may be out of credit as well,
    /// this grant is the last chance, and one that failed before is tried
    /// again regardless.
    fn grant_due(&mut self, all: bool, blocking: bool) -> Option<GrantToken> {
        let mut state = self.stream.lock();
        if state.owed() == 0 {
            return None;
        }
        let last_chance = blocking && state.producer_may_be_starved();
        if all {
            // One retry per call: the failure is likely still there, and
            // a loop that retried at once would not leave this call.
            if state.grant_failed && !last_chance && !std::mem::take(&mut self.retry_armed) {
                return None;
            }
        } else if state.owed() < (self.window as u64 / 2).clamp(1, state.declared.max(1) as u64) {
            return None;
        }
        let source = state.source.clone()?;
        let total = state.drained;
        // Counted when tried, not when sent — and once, being a total.
        state.announced = state.announced.max(total);
        Some(GrantToken {
            stream: self.stream.clone(),
            source,
            total,
            last_chance: all && last_chance,
        })
    }

    /// The terminator has been reached and every item before it handed
    /// out: report it once, then read as finished.
    fn finish(&mut self) -> Option<BinderResult<T>> {
        self.finished = true;
        let end = self.stream.lock().end.clone();
        match end {
            Some(status) if !status.is_ok() => Some(Err(status)),
            _ => None,
        }
    }
}

impl<T: Deserialize> Iterator for Receiver<T> {
    type Item = BinderResult<T>;

    fn next(&mut self) -> Option<Self::Item> {
        self.recv()
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        let (source, binder, death) = {
            let mut state = self.stream.lock();
            state.canceled = true;
            state.closed = true;
            state.batches.clear();
            (
                state.source.take(),
                state.source_binder.take(),
                state.death.take(),
            )
        };
        if let Some(binder) = binder {
            // Dropping the recipient only makes the link inert; the
            // source's recipient list never shrinks on its own.
            unlink_death(&binder, &death);
        }
        if let Some(source) = source {
            // A producer parked on `wait_credit` has no other way to
            // learn that nobody is reading any more.
            let _ = source.cancel();
        }
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Receiver")
            .field("window", &self.window)
            .field("finished", &self.finished)
            .field("started", &self.stream.lock().source.is_some())
            .finish()
    }
}

impl Peer<dyn IStreamSource> {
    fn request(&self, total: i64) -> Result<()> {
        match self {
            Peer::Typed(source) => source.r#request(total).map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data =
                    proxy.build_request(<BpStreamSource as crate::Proxy>::descriptor())?;
                data.write(&total)?;
                proxy
                    .transact(
                        generated::rsbinder::stream::IStreamSource::transactions::r#request,
                        &data,
                        ONEWAY_FLAGS,
                    )
                    .map(|_| ())
            }
        }
    }

    fn cancel(&self) -> Result<()> {
        match self {
            Peer::Typed(source) => source.r#cancel().map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let data = proxy.build_request(<BpStreamSource as crate::Proxy>::descriptor())?;
                proxy
                    .transact(
                        generated::rsbinder::stream::IStreamSource::transactions::r#cancel,
                        &data,
                        ONEWAY_FLAGS,
                    )
                    .map(|_| ())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;

    /// A producer and a consumer in one process. The sink binder is local,
    /// so no transport is involved; the wire is `tests/stream_rpc.rs`.
    fn pair<T: Serialize + Deserialize>(
        window: u32,
        max_batch_bytes: usize,
        initial_credits: u32,
    ) -> (Sink<T>, Receiver<T>) {
        let (rx, sink_binder) = Receiver::<T>::with_credit_window(window);
        let sink = Sink::<T>::with_limits(&sink_binder, max_batch_bytes, initial_credits)
            .expect("a local binder can always be called back");
        (sink, rx)
    }

    #[derive(Default)]
    struct Recorded {
        /// How many `onBatch` calls to refuse; the next one clears it.
        refuse: AtomicUsize,
        /// Refuse the way a transport does when it cannot say whether the
        /// call arrived, rather than the way the driver refuses one.
        unknown: std::sync::atomic::AtomicBool,
        batches: AtomicUsize,
        end: Mutex<Option<(i32, Option<String>)>>,
        ends: AtomicUsize,
    }

    /// A sink that refuses batches on request and records its terminator.
    struct RefusingSink(Arc<Recorded>);

    impl Interface for RefusingSink {}

    impl IStreamSink for RefusingSink {
        fn r#onStart(&self, _source: &SIBinder, _credits: i32) -> BinderResult<()> {
            Ok(())
        }
        fn r#onBatch(&self, _items: &[u8], _count: i32) -> BinderResult<()> {
            if self.0.refuse.swap(0, Ordering::SeqCst) > 0 {
                return Err(Status::from(if self.0.unknown.load(Ordering::SeqCst) {
                    StatusCode::Unknown
                } else {
                    StatusCode::FailedTransaction
                }));
            }
            self.0.batches.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
        fn r#onEnd(
            &self,
            exception: i32,
            _service_specific: i32,
            message: Option<&str>,
        ) -> BinderResult<()> {
            *self.0.end.lock().unwrap_or_else(|e| e.into_inner()) =
                Some((exception, message.map(str::to_owned)));
            self.0.ends.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[derive(Default)]
    struct SourceCalls {
        /// How many `request` calls to refuse, counted down.
        refuse: AtomicUsize,
        attempts: AtomicUsize,
        /// The highest running total a `request` was accepted with.
        granted: AtomicUsize,
        canceled: AtomicUsize,
    }

    /// A source that refuses grants on request and counts what it gets.
    struct RefusingSource(Arc<SourceCalls>);

    impl Interface for RefusingSource {}

    impl IStreamSource for RefusingSource {
        fn r#request(&self, total: i64) -> BinderResult<()> {
            self.0.attempts.fetch_add(1, Ordering::SeqCst);
            let refused = self
                .0
                .refuse
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
            if refused.is_ok() {
                return Err(Status::from(ExceptionCode::IllegalState));
            }
            self.0.granted.fetch_max(total as usize, Ordering::SeqCst);
            Ok(())
        }
        fn r#cancel(&self) -> BinderResult<()> {
            self.0.canceled.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    /// A receiver whose producer has introduced itself with `credits`,
    /// the producer being a source this test drives by hand.
    fn started<T: Deserialize>(
        window: u32,
        max_opening: u32,
        credits: i32,
    ) -> (Receiver<T>, Strong<dyn IStreamSink>, Arc<SourceCalls>) {
        let calls = Arc::new(SourceCalls::default());
        let source = BnStreamSource::new_binder(RefusingSource(calls.clone())).as_binder();
        let (rx, sink_binder) = Receiver::<T>::with_limits(window, max_opening);
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");
        sink.r#onStart(&source, credits).expect("onStart");
        (rx, sink, calls)
    }

    /// The ways a batch or a grant can leave the code that made it.
    #[derive(Clone, Copy, Debug)]
    enum WayOut {
        Accepted,
        Refused,
        /// What a pool task the runtime drops without running amounts to.
        DroppedUnsent,
        Unwound,
    }

    const WAYS_OUT: [WayOut; 4] = [
        WayOut::Accepted,
        WayOut::Refused,
        WayOut::DroppedUnsent,
        WayOut::Unwound,
    ];

    fn unwind_past<V>(value: V) {
        let unwound = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
            let _held = value;
            std::panic::resume_unwind(Box::new("unwinding past a token"));
        }));
        assert!(unwound.is_err());
    }

    /// An item that left is either delivered or counted as lost, whichever
    /// way the batch goes. Its credit comes back when the batch is known
    /// not to have arrived. When that is not known there is no right
    /// answer — given back, it may be credit the consumer never issued —
    /// so the stream is marked broken and sends no more.
    #[test]
    fn a_batch_in_transit_settles_on_every_way_out() {
        #[derive(Clone, Copy, Debug)]
        enum Way {
            Accepted,
            RefusedByTheDriver,
            FailedWithoutSayingWhich,
            DroppedUnsent,
            UnwoundUnsent,
            UnwoundInTheCall,
        }
        for way in [
            Way::Accepted,
            Way::RefusedByTheDriver,
            Way::FailedWithoutSayingWhich,
            Way::DroppedUnsent,
            Way::UnwoundUnsent,
            Way::UnwoundInTheCall,
        ] {
            let recorded = Arc::new(Recorded::default());
            let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
            let peer = resolve_peer::<dyn IStreamSink>(
                &binder,
                <BpStreamSink as crate::Proxy>::descriptor(),
                "test",
            )
            .expect("a local sink");
            let credit = Arc::new(Credit::default());
            credit.grant(1);
            let ledger = Arc::new(Ledger::default());

            assert!(credit.try_credit());
            let mut batch = BatchInTransit::new(credit.clone(), ledger.clone(), 3);
            batch.bytes = vec![0; 12];
            assert!(ledger.lock().in_transit, "{way:?}");

            match way {
                Way::Accepted => batch.send(&peer),
                Way::RefusedByTheDriver => {
                    recorded.refuse.store(1, Ordering::SeqCst);
                    batch.send(&peer);
                }
                Way::FailedWithoutSayingWhich => {
                    recorded.unknown.store(true, Ordering::SeqCst);
                    recorded.refuse.store(1, Ordering::SeqCst);
                    batch.send(&peer);
                }
                Way::DroppedUnsent => drop(batch),
                Way::UnwoundUnsent => unwind_past(batch),
                Way::UnwoundInTheCall => {
                    // What `send` sets before it calls out.
                    batch.outcome = Outcome::Unknown(StatusCode::Unknown);
                    unwind_past(batch);
                }
            }

            let delivered = matches!(way, Way::Accepted);
            let unknown = matches!(way, Way::FailedWithoutSayingWhich | Way::UnwoundInTheCall);
            assert!(!ledger.lock().in_transit, "{way:?}: nothing may wait on it");
            assert_eq!(
                credit.lock().available,
                u64::from(!delivered && !unknown),
                "{way:?}: credit comes back only for a batch known not to have arrived"
            );
            assert_eq!(ledger.lost(), if delivered { 0 } else { 3 }, "{way:?}");
            assert_eq!(ledger.broken().is_some(), unknown, "{way:?}");
            assert_eq!(
                ledger.take_unreported().is_some(),
                !delivered,
                "{way:?}: the next call has a failure to return"
            );
        }
    }

    /// What a failure says about the batch depends on what carried it. A
    /// deadline over RPC is a frame that did not go out whole; the same
    /// code from the kernel path is an unrelated command's (`BR_FINISHED`).
    #[test]
    fn what_a_failed_send_says_about_the_batch_depends_on_the_transport() {
        for (e, kernel, rpc) in [
            (StatusCode::FailedTransaction, true, true),
            (StatusCode::DeadObject, true, true),
            (StatusCode::TimedOut, false, true),
            (StatusCode::Unknown, false, false),
            (StatusCode::BadValue, false, false),
        ] {
            assert_eq!(
                certainly_not_delivered(e, false),
                kernel,
                "{e:?} from the kernel"
            );
            assert_eq!(certainly_not_delivered(e, true), rpc, "{e:?} over RPC");
        }
    }

    /// A send that cannot say whether its batch arrived ends the sink: the
    /// consumer holds the producer to its credit, and how much is left is
    /// no longer known. The terminator spends none, so it still goes out.
    #[test]
    fn a_send_that_cannot_say_whether_it_arrived_ends_the_sink() {
        let recorded = Arc::new(Recorded::default());
        recorded.unknown.store(true, Ordering::SeqCst);
        recorded.refuse.store(1, Ordering::SeqCst);
        let sink_binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        // Eight bytes to a batch: every second `i32` is what sends one.
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 8, 4).expect("with_limits");

        sink.send(&0).expect("queued");
        assert_eq!(sink.send(&1).err(), Some(StatusCode::Unknown));
        assert_eq!(
            sink.send(&2).err(),
            Some(StatusCode::Unknown),
            "every later send reports it, one that would only have queued too"
        );
        assert_eq!(
            sink.pending(),
            0,
            "and queues nothing, since nothing will send it"
        );
        assert_eq!(sink.flush().err(), Some(StatusCode::Unknown));
        assert_eq!(
            sink.credits(),
            3,
            "the credit is neither spent twice nor returned"
        );
        assert_eq!(sink.end().err(), Some(StatusCode::Unknown));

        assert_eq!(
            recorded.batches.load(Ordering::SeqCst),
            0,
            "nothing more was sent"
        );
        let (exception, message) = recorded
            .end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("a terminator arrived");
        assert_eq!(exception, ExceptionCode::IllegalState as i32);
        assert!(
            message.unwrap_or_default().contains("2 queued item"),
            "the two items of the batch that may or may not have arrived"
        );
    }

    /// A grant is a running total, so however it goes — sent, refused, or
    /// carried by a pool task that never ran — it is counted once: what
    /// the producer may have been given does not grow with each try, and
    /// what is owed stays owed until a grant is sent. A refusal with
    /// nothing left to prompt a retry ends the stream instead: that is a
    /// consumer about to block on a producer that may have no credit.
    #[test]
    fn a_grant_settles_on_every_way_out() {
        for last_chance in [false, true] {
            for way in WAYS_OUT {
                let (mut rx, _sink, calls) = started::<i32>(2, 4, 4);
                {
                    let mut state = rx.stream.lock();
                    state.drained = 3;
                    // Every credit the producer is known to have, used.
                    state.received = if last_chance { 4 } else { 3 };
                }

                let grant = rx.grant_due(true, true).expect("three batches are owed");
                assert_eq!(grant.total, 3);
                assert_eq!(grant.last_chance, last_chance);

                match way {
                    WayOut::Accepted => grant.send(),
                    WayOut::Refused => {
                        calls.refuse.store(1, Ordering::SeqCst);
                        grant.send();
                    }
                    WayOut::DroppedUnsent => drop(grant),
                    WayOut::Unwound => unwind_past(grant),
                }

                let case = format!("{way:?}, last chance {last_chance}");
                let sent = matches!(way, WayOut::Accepted);
                let refused = matches!(way, WayOut::Refused);
                let gave_up = last_chance && refused;
                let settled = |rx: &Receiver<i32>, again: &str| {
                    let state = rx.stream.lock();
                    assert_eq!(
                        state.issued(),
                        7,
                        "{case}{again}: a total is counted once however often it is tried"
                    );
                    assert_eq!(state.granted, if sent { 3 } else { 0 }, "{case}{again}");
                    assert_eq!(state.owed(), if sent { 0 } else { 3 }, "{case}{again}");
                };
                settled(&rx, "");
                assert_eq!(
                    rx.stream.lock().grant_failed,
                    refused && !gave_up,
                    "{case}: only a refusal holds the next try back"
                );
                assert_eq!(rx.end_status().is_some(), gave_up, "{case}");
                assert_eq!(
                    calls.canceled.load(Ordering::SeqCst),
                    usize::from(gave_up),
                    "{case}: a stream the consumer ends releases its producer"
                );

                // Tried again and refused again: the bound does not move.
                if !sent && !gave_up {
                    calls.refuse.store(usize::MAX, Ordering::SeqCst);
                    rx.stream.lock().received = 3;
                    rx.retry_armed = true;
                    rx.grant_due(true, false).expect("still owed").send();
                    settled(&rx, ", tried again");
                }
            }
        }
    }

    /// The producer's half of the same rule: the highest total seen is
    /// what counts, so a grant that arrives twice is worth what it was
    /// worth once.
    #[test]
    fn a_running_total_is_counted_once_by_the_producer() {
        let credit = Credit::default();
        credit.grant(2);
        for (total, available) in [(4, 6), (4, 6), (6, 8), (3, 8), (0, 8), (7, 9)] {
            credit.granted_up_to(total);
            assert_eq!(
                credit.lock().available,
                available,
                "after a total of {total}"
            );
        }
    }

    /// A grant whose pool task never ran holds nothing back: what it would
    /// have announced is still owed, and the next call announces it.
    #[test]
    fn a_grant_nobody_sent_is_sent_by_the_next_call() {
        let (mut rx, _sink, calls) = started::<i32>(2, 4, 4);
        rx.stream.lock().drained = 1;
        drop(rx.grant_due(false, false).expect("one batch is owed"));
        assert_eq!(calls.attempts.load(Ordering::SeqCst), 0);

        assert_eq!(rx.try_recv().expect("no error"), None);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 1);
    }

    /// A terminator reaches the consumer exactly once, whichever way it
    /// goes: one that goes nowhere leaves the consumer blocked with both
    /// processes alive, so no death link ends the wait either.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_terminator_in_transit_is_sent_on_every_way_out() {
        for way in [WayOut::Accepted, WayOut::DroppedUnsent, WayOut::Unwound] {
            let recorded = Arc::new(Recorded::default());
            let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
            let peer = resolve_peer::<dyn IStreamSink>(
                &binder,
                <BpStreamSink as crate::Proxy>::descriptor(),
                "test",
            )
            .expect("a local sink");
            let outcome = Arc::new(Mutex::new(None));
            let terminator = TerminatorInTransit {
                peer: Arc::new(peer),
                exception: ExceptionCode::None as i32,
                service_specific: 0,
                message: None,
                outcome: outcome.clone(),
                sent: false,
            };

            match way {
                WayOut::Accepted => terminator.send(),
                WayOut::DroppedUnsent => drop(terminator),
                WayOut::Unwound => unwind_past(terminator),
                WayOut::Refused => unreachable!(),
            }
            assert_eq!(recorded.ends.load(Ordering::SeqCst), 1, "{way:?}");
            assert_eq!(
                outcome.lock().unwrap_or_else(|e| e.into_inner()).take(),
                Some(Ok(())),
                "{way:?}: the caller is told what the send came to, not where it ran"
            );
        }
    }

    /// `spawn_blocking` queues the task before it finds the OS will not
    /// give it a thread, and then panics: the task is neither run nor
    /// dropped. Nothing may be left waiting on a token inside it.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_hand_off_that_queues_and_unwinds_gives_the_token_back() {
        let credit = Arc::new(Credit::default());
        let ledger = Arc::new(Ledger::default());
        credit.grant(1);
        assert!(credit.try_credit());
        let batch = BatchInTransit::new(credit.clone(), ledger.clone(), 2);

        let ran = Arc::new(AtomicUsize::new(0));
        let counter = ran.clone();
        let mut queued = None;
        let handed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            carry(
                batch,
                move |batch: BatchInTransit| {
                    counter.fetch_add(1, Ordering::SeqCst);
                    drop(batch);
                    Ok(())
                },
                |task| {
                    queued = Some(task);
                    std::panic::resume_unwind(Box::new("no thread to run it"));
                },
            )
        }));
        assert!(handed.is_err());

        assert!(
            !ledger.lock().in_transit,
            "or `Drop for Sink` waits forever"
        );
        assert_eq!(credit.lock().available, 1);
        assert_eq!(ledger.lost(), 2);

        // The queue is drained later, by a spawn that does find a thread.
        queued.expect("queued").run().expect("a task with no token");
        assert_eq!(ran.load(Ordering::SeqCst), 0, "the token settled once");
        assert_eq!(ledger.lost(), 2);
    }

    /// A pool that drops the terminator's task unrun says the task
    /// failed. The terminator went out all the same, from the token's
    /// `Drop`, and that is what the caller asked about.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_the_pool_drops_unrun_reports_the_send_not_the_pool() {
        let recorded = Arc::new(Recorded::default());
        let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        let sink = Sink::<i32>::new(&binder).expect("a local sink");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let handle = runtime.handle().clone();
        runtime.shutdown_background();

        assert_eq!(handle.block_on(sink.end_async()), Ok(()));
        assert_eq!(recorded.ends.load(Ordering::SeqCst), 1);
    }

    /// `end_async` polled where the pool cannot take the terminator: the
    /// stream is already marked ended by then, so `Drop` will not send
    /// one, and it must not be the case that nothing does.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_the_pool_cannot_take_still_terminates_the_stream() {
        let recorded = Arc::new(Recorded::default());
        let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        let sink = Sink::<i32>::new(&binder).expect("a local sink");

        // No runtime here, so `spawn_blocking` panics with the call in hand.
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut end = std::pin::pin!(sink.end_async());
            let mut context = std::task::Context::from_waker(std::task::Waker::noop());
            let _ = std::future::Future::poll(end.as_mut(), &mut context);
        }));
        assert!(polled.is_err(), "there is no pool to hand the call to");
        assert_eq!(
            recorded.ends.load(Ordering::SeqCst),
            1,
            "the terminator goes out once, from wherever the call was dropped"
        );
    }

    /// A consumer driven on a runtime whose pool is gone cannot send the
    /// credit it owes. It has to say so: parking would wait on a producer
    /// that is out of credit, with nothing left to wake either of them.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_consumer_whose_pool_is_gone_ends_the_stream() {
        let calls = Arc::new(SourceCalls::default());
        let source = BnStreamSource::new_binder(RefusingSource(calls.clone())).as_binder();
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(2);
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");
        sink.r#onStart(&source, 4).expect("onStart");
        rx.stream.lock().drained = 1;

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let handle = runtime.handle().clone();
        runtime.shutdown_background();

        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let last = handle.block_on(rx.recv_async());
            // Sent back alive: dropping the receiver cancels too, and
            // would race the count below.
            let _ = done.send((last, rx));
        });
        let (last, _rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the consumer must neither park nor spin");
        assert!(matches!(last, Some(Err(_))), "{last:?}");
        assert_eq!(
            calls.canceled.load(Ordering::SeqCst),
            1,
            "the producer is released, from this thread since no pool will"
        );
    }

    #[test]
    fn items_arrive_in_order_and_the_stream_ends_clean() {
        let (mut sink, mut rx) = pair::<i32>(
            DEFAULT_CREDIT_WINDOW,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
        );
        let sent: Vec<i32> = (0..10).collect();
        sink.send_all(sent.iter()).expect("send_all");
        sink.end().expect("end");

        let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, sent);
        assert!(rx.is_finished());
        assert!(
            rx.end_status().expect("a terminator arrived").is_ok(),
            "a stream that ran out ends with no exception"
        );
    }

    /// Plan 10-7 AC-7.2. Four bytes per batch and one opening credit, so
    /// the second item has nowhere to go until the consumer drains the
    /// first.
    #[test]
    fn a_full_window_stops_the_producer_until_the_consumer_drains() {
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");

        let (progress, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            for item in 0..4i32 {
                if sink.send(&item).is_err() {
                    return;
                }
                let _ = progress.send(item);
            }
            let _ = sink.end();
        });

        assert_eq!(
            watch
                .recv_timeout(Duration::from_secs(5))
                .expect("the opening credit carries the first item"),
            0
        );
        assert!(
            watch.recv_timeout(Duration::from_millis(200)).is_err(),
            "the second item must wait: nothing has granted credit for it"
        );

        let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, vec![0, 1, 2, 3]);
        producer.join().expect("producer thread");
    }

    /// The window is what the producer may have in flight, not twice it.
    /// Counted, because "the producer stops somewhere" holds for a window
    /// of any size: one batch drained buys exactly one batch more.
    #[test]
    fn draining_a_batch_buys_exactly_one_batch_more() {
        // A window of two puts the grant threshold at one batch, so every
        // drained batch is paid back at once; four bytes to a batch makes
        // a batch and an `i32` the same thing.
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(2);
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");

        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let producer = thread::spawn(move || {
            for item in 0..1000i32 {
                if sink.send(&item).is_err() {
                    return;
                }
                counter.fetch_add(1, Ordering::SeqCst);
            }
        });

        const DRAINED: usize = 3;
        for expected in 0..DRAINED as i32 {
            assert_eq!(rx.next().expect("an item").expect("item"), expected);
        }
        let allowed = 1 + DRAINED;

        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.load(Ordering::SeqCst) < allowed {
            assert!(
                Instant::now() < deadline,
                "the producer has credit for {allowed} batches and has sent {}",
                sent.load(Ordering::SeqCst)
            );
            thread::sleep(Duration::from_millis(5));
        }
        // Long enough for a surplus grant to have been spent.
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            sent.load(Ordering::SeqCst),
            allowed,
            "the opening credit plus one per drained batch is the whole budget"
        );

        rx.cancel().expect("cancel");
        producer.join().expect("producer thread");
    }

    /// The failure a service would have returned, arriving after the
    /// method that started the stream already succeeded.
    #[test]
    fn a_service_specific_failure_survives_the_terminator() {
        let (mut sink, mut rx) = pair::<i32>(
            DEFAULT_CREDIT_WINDOW,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
        );
        sink.send(&7).expect("send");
        sink.end_with(&Status::new_service_specific_error(
            42,
            Some("quota exhausted".to_string()),
        ))
        .expect("end_with");

        assert_eq!(rx.next().expect("an item").expect("item"), 7);
        let failure = rx.next().expect("the failure").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::ServiceSpecific);
        assert_eq!(failure.service_specific_error(), 42);
        assert_eq!(failure.message(), Some("quota exhausted"));
        assert!(
            rx.next().is_none(),
            "the failure is reported once and the stream is then over"
        );
    }

    /// A producer whose opening window is smaller than the consumer's
    /// grant threshold runs out of credit before the threshold is ever
    /// reached. The consumer has to grant what it owes before it waits,
    /// or both ends wait on each other.
    #[test]
    fn an_opening_window_below_the_grant_threshold_does_not_stall() {
        // Threshold is half of eight; the producer opens with one.
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(8);
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");

        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let sent = sink.send_all((0..20).collect::<Vec<i32>>().iter());
            let _ = done.send(sent.and_then(|()| sink.end()));
        });

        let mut got = Vec::new();
        while got.len() < 20 {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(Some(item)) => got.push(item),
                other => panic!("stalled after {} items: {other:?}", got.len()),
            }
        }
        assert_eq!(got, (0..20).collect::<Vec<_>>());
        watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the producer must finish")
            .expect("send and end");
    }

    /// A consumer that gave up before the producer got as far as
    /// introducing itself is answered the moment it does.
    #[test]
    fn a_receiver_dropped_before_the_stream_starts_cancels_it_on_arrival() {
        let (rx, sink_binder) = Receiver::<i32>::new();
        drop(rx);

        let mut sink = Sink::<i32>::new(&sink_binder).expect("the sink object is still there");
        assert!(sink.is_canceled(), "`onStart` must have been answered");
        assert_eq!(
            sink.send(&1).and_then(|()| sink.flush()).err(),
            Some(StatusCode::InvalidOperation)
        );
    }

    /// An async producer waiting for credit gives the thread back. On a
    /// current-thread runtime that is the difference between finishing
    /// and deadlock: the consumer task that would grant the credit runs
    /// on the same thread the producer would otherwise be parked on.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_async_producer_waits_for_credit_without_holding_the_thread() {
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let got = runtime.block_on(async {
                let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
                let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");
                let credit_spent = Arc::new(tokio::sync::Notify::new());
                let spent = credit_spent.clone();
                // `tokio::spawn` is also the check that the futures are
                // `Send`.
                let producer = tokio::spawn(async move {
                    // Spends the only credit.
                    sink.send_async(&0).await?;
                    spent.notify_one();
                    // So this one finds none, and nothing is reading yet.
                    for item in 1..50i32 {
                        sink.send_async(&item).await?;
                    }
                    sink.end_async().await
                });
                // Held back until the producer is out of credit, so its
                // next send has to wait rather than find a fresh grant.
                credit_spent.notified().await;
                let mut got = Vec::new();
                while let Some(item) = rx.recv_async().await {
                    got.push(item.expect("item"));
                }
                producer.await.expect("join").expect("send and end");
                got
            });
            let _ = done.send(got);
        });
        assert_eq!(
            watch
                .recv_timeout(Duration::from_secs(10))
                .expect("the stream must finish on a single thread"),
            (0..50).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_cancel_releases_a_producer_waiting_for_credit() {
        let (rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");

        let (outcome, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            // The first send spends the opening credit; the second parks.
            let first = sink.send(&0);
            let second = sink.send(&1);
            let _ = outcome.send((first, second, sink.is_canceled()));
        });

        // Nothing drains, so the producer is parked. Cancel is the only
        // thing that can release it.
        assert!(
            watch.recv_timeout(Duration::from_millis(200)).is_err(),
            "the producer must be waiting for credit"
        );
        rx.cancel().expect("cancel");

        let (first, second, canceled) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel must release the producer");
        assert!(first.is_ok());
        assert_eq!(second.err(), Some(StatusCode::InvalidOperation));
        assert!(
            canceled,
            "`is_canceled` is what tells that refusal from an unusable transport"
        );
        producer.join().expect("producer thread");
    }

    /// A producer that goes away mid-stream still delivers what it had
    /// and still ends the stream, because the consumer blocked in `recv`
    /// has no other way to find out.
    #[test]
    fn a_dropped_sink_flushes_and_reports_that_it_never_ended() {
        let (mut sink, mut rx) = pair::<i32>(
            DEFAULT_CREDIT_WINDOW,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
        );
        sink.send(&1).expect("send");
        sink.send(&2).expect("send");
        assert_eq!(sink.pending(), 2, "neither item filled a 16 KB batch");
        drop(sink);

        assert_eq!(rx.next().expect("item 1").expect("ok"), 1);
        assert_eq!(rx.next().expect("item 2").expect("ok"), 2);
        let failure = rx.next().expect("a terminator").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(rx.next().is_none(), "the stream is over");
    }

    /// The same drop with no credit left: the queued item cannot go out,
    /// and waiting for a grant would park whichever thread is dropping.
    #[test]
    fn a_dropped_sink_with_no_credit_still_ends_the_stream() {
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
        // Eight bytes to a batch is two `i32`s, and one opening credit.
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 8, 1).expect("with_limits");

        sink.send(&0).expect("send");
        // Fills the batch, so this one spends the only credit.
        sink.send(&1).expect("send");
        // Queued with nothing left to send it, and nothing draining yet,
        // so no grant is coming either.
        sink.send(&2).expect("send");
        assert_eq!(sink.credits(), 0);
        assert_eq!(sink.pending(), 1);

        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            drop(sink);
            let _ = done.send(());
        });
        watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the drop must not wait for credit");

        assert_eq!(rx.next().expect("item 0").expect("ok"), 0);
        assert_eq!(rx.next().expect("item 1").expect("ok"), 1);
        let failure = rx.next().expect("a terminator").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(
            failure
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the terminator says what was lost: {failure:?}"
        );
    }

    /// A batch that failed earlier in the stream is still counted by the
    /// terminator: a clean `EX_NONE` over lost items reads exactly like a
    /// stream that ran out.
    #[test]
    fn items_a_failed_batch_lost_are_reported_by_the_terminator() {
        let recorded = Arc::new(Recorded::default());
        recorded.refuse.store(1, Ordering::SeqCst);
        let sink_binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        // Four bytes to a batch, so one `i32` is one batch, and enough
        // credit that nothing here waits.
        let mut sink = Sink::<i32>::with_limits(&sink_binder, 4, 4).expect("with_limits");

        sink.send(&0).expect_err("the first batch is refused");
        // The failure cost that item and nothing else: the stream is
        // still usable, as `send`'s rustdoc says.
        sink.send(&1).expect("send");
        sink.end().expect("end");

        let (exception, message) = recorded
            .end
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .expect("a terminator arrived");
        assert_eq!(
            exception,
            ExceptionCode::IllegalState as i32,
            "a stream that lost an item must not end on EX_NONE"
        );
        assert!(
            message.unwrap_or_default().contains("1 queued item"),
            "the terminator says how many items went missing"
        );
    }

    /// `EX_TRANSACTION_FAILED` says the binder layer failed, which a call
    /// that arrived cannot report, so the producer's own status is
    /// refused — and the stream still ends, on `EX_ILLEGAL_ARGUMENT`.
    #[test]
    fn a_terminator_that_cannot_be_carried_is_refused() {
        let (sink, mut rx) = pair::<i32>(
            DEFAULT_CREDIT_WINDOW,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
        );
        assert_eq!(
            sink.end_with(&Status::from(ExceptionCode::TransactionFailed))
                .err(),
            Some(StatusCode::BadValue)
        );
        let ended = rx.next().expect("a terminator").expect_err("an error");
        assert_eq!(ended.exception_code(), ExceptionCode::IllegalArgument);
        assert_eq!(ended.message(), Some(UNCARRIABLE_TERMINATOR));
    }

    #[test]
    fn an_opening_window_of_nothing_is_refused() {
        let (_rx, sink_binder) = Receiver::<i32>::new();
        assert_eq!(
            Sink::<i32>::with_limits(&sink_binder, DEFAULT_MAX_BATCH_BYTES, 0).err(),
            Some(StatusCode::BadValue)
        );
    }

    #[test]
    fn a_binder_that_is_not_a_sink_is_refused() {
        let not_a_sink =
            BnStreamSource::new_binder(SourceObject(Arc::new(Credit::default()))).as_binder();
        assert_eq!(
            Sink::<i32>::new(&not_a_sink).err(),
            Some(StatusCode::BadType)
        );
    }

    /// A batch whose stated count does not match its bytes is refused
    /// rather than guessed at, and the items decoded before the mismatch
    /// are not handed out.
    #[test]
    fn a_batch_that_claims_more_items_than_it_carries_is_refused() {
        let (mut rx, sink, _calls) = started::<i32>(DEFAULT_CREDIT_WINDOW, 4, 4);

        sink.r#onBatch(&[1, 0, 0, 0], 2).expect("onBatch");
        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");

        assert!(
            rx.next().expect("a report").is_err(),
            "one i32 cannot be two items"
        );
        assert!(
            rx.next().is_none(),
            "the half-decoded batch must not surface after the error"
        );
    }

    /// `onBatch` is `oneway`, so the error a refused batch returns is
    /// logged and dropped. The consumer has to be told some other way, or
    /// a peer that skipped a batch reads as one that sent every item.
    #[test]
    fn a_batch_with_a_negative_count_ends_the_stream() {
        let credit = Arc::new(Credit::default());
        let source = BnStreamSource::new_binder(SourceObject(credit.clone())).as_binder();
        let (mut rx, sink_binder) = Receiver::<i32>::new();
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");
        sink.r#onStart(&source, 4).expect("onStart");

        assert!(sink.r#onBatch(&[1, 0, 0, 0], -1).is_err());
        assert!(
            credit.is_canceled(),
            "no more credit is coming, so only a cancel releases the producer"
        );
        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");

        assert_eq!(
            rx.next()
                .expect("a report")
                .expect_err("a clean terminator must not overwrite it")
                .exception_code(),
            ExceptionCode::IllegalArgument
        );
    }

    /// Credit is what bounds the consumer's queue, so a producer is held
    /// to the credit it has: its opening window, plus what was granted.
    #[test]
    fn a_batch_sent_without_credit_ends_the_stream() {
        // A window of two with one declared puts the threshold at one, so
        // the batch drained below is paid for at once.
        let (mut rx, sink, calls) = started::<i32>(2, 4, 1);

        sink.r#onBatch(&[1, 0, 0, 0], 1)
            .expect("the opening credit");
        assert_eq!(rx.next().expect("a batch").expect("an item"), 1);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 1);
        sink.r#onBatch(&[2, 0, 0, 0], 1)
            .expect("the granted credit");

        assert!(
            sink.r#onBatch(&[3, 0, 0, 0], 1).is_err(),
            "one opening credit and one granted have both been spent"
        );
        assert_eq!(
            calls.canceled.load(Ordering::SeqCst),
            1,
            "the producer is told, or it parks on credit that is not coming"
        );
        // Polled, not waited for: without the refusal there is nothing
        // to wait for, and the test would hang rather than fail.
        assert_eq!(rx.try_recv().expect("the batch before it"), Some(2));
        assert_eq!(
            rx.try_recv().expect_err("an error").exception_code(),
            ExceptionCode::IllegalState
        );
    }

    /// `oneway` calls to one object arrive in order and a producer sends
    /// `onStart` first, so a batch ahead of it is a producer with no
    /// credit at all.
    #[test]
    fn a_batch_before_the_producer_introduces_itself_is_refused() {
        let (mut rx, sink_binder) = Receiver::<i32>::new();
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");

        assert!(sink.r#onBatch(&[1, 0, 0, 0], 1).is_err());
        assert_eq!(
            rx.try_recv().expect_err("an error").exception_code(),
            ExceptionCode::IllegalState
        );
    }

    /// The consumer sets how wide a window it takes, and says no before
    /// any batch rather than part-way through the stream.
    #[test]
    fn an_opening_window_the_consumer_does_not_accept_is_refused_at_the_start() {
        for credits in [0, -1, 5, i32::MAX] {
            let (mut rx, _sink, calls) = started::<i32>(DEFAULT_CREDIT_WINDOW, 4, credits);
            assert_eq!(
                rx.try_recv().expect_err("an error").exception_code(),
                ExceptionCode::IllegalArgument,
                "{credits}"
            );
            assert_eq!(calls.canceled.load(Ordering::SeqCst), 1, "{credits}");
        }

        let (rx, _sink, calls) = started::<i32>(DEFAULT_CREDIT_WINDOW, 8, 5);
        assert!(
            rx.end_status().is_none(),
            "a consumer set up for it takes it"
        );
        assert_eq!(calls.canceled.load(Ordering::SeqCst), 0);

        // The producer's side of the same refusal.
        let (_rx, sink_binder) = Receiver::<i32>::new();
        let sink = Sink::<i32>::with_limits(&sink_binder, DEFAULT_MAX_BATCH_BYTES, 5)
            .expect("`onStart` is oneway, so the refusal is not an error here");
        assert!(sink.is_canceled());
    }

    /// A second `onStart` is somebody else's mistake: whatever it states,
    /// it must not end the stream that is running.
    #[test]
    fn a_second_on_start_cannot_end_the_stream() {
        let (rx, sink, _calls) = started::<i32>(DEFAULT_CREDIT_WINDOW, 4, 4);
        let other = Arc::new(SourceCalls::default());
        let intruder = BnStreamSource::new_binder(RefusingSource(other.clone())).as_binder();

        sink.r#onStart(&intruder, i32::MAX).expect("onStart");

        assert!(rx.end_status().is_none());
        assert_eq!(other.canceled.load(Ordering::SeqCst), 1);

        // Nor one whose source is not a source at all.
        let (_other_rx, not_a_source) = Receiver::<i32>::new();
        sink.r#onStart(&not_a_source, 4).expect("onStart");
        assert!(rx.end_status().is_none());
    }

    /// With more queued, a grant waits for half a window to be owed —
    /// but never for more than the producer opened with, which it could
    /// not send enough batches to reach.
    #[test]
    fn the_grant_threshold_is_held_to_the_opening_window() {
        // Half of eight is four; the producer opened with two.
        let (mut rx, sink, calls) = started::<i32>(8, 4, 2);
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");
        sink.r#onBatch(&[2, 0, 0, 0], 1).expect("onBatch");

        assert_eq!(rx.next().expect("a batch").expect("an item"), 1);
        assert_eq!(
            calls.attempts.load(Ordering::SeqCst),
            0,
            "one owed, one still queued"
        );
        assert_eq!(rx.next().expect("a batch").expect("an item"), 2);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 2);
    }

    /// A producer can make every grant fail — its own buffer for `oneway`
    /// calls kept full — while its process stays alive, so no death link
    /// fires. However often the consumer retries, what the producer may
    /// send does not grow: the bound on the queue is the opening window.
    #[test]
    fn grants_that_keep_failing_do_not_raise_what_the_producer_may_send() {
        let (mut rx, sink, calls) = started::<i32>(2, 4, 4);
        calls.refuse.store(usize::MAX, Ordering::SeqCst);
        for item in 0..4u8 {
            sink.r#onBatch(&[item, 0, 0, 0], 1)
                .expect("the opening window");
        }
        for item in 0..4 {
            assert_eq!(rx.try_recv().expect("no error"), Some(item));
        }
        // A service's event loop, polling a stream that has gone quiet.
        for _ in 0..100 {
            assert_eq!(rx.try_recv().expect("the stream goes on"), None);
        }
        assert!(calls.attempts.load(Ordering::SeqCst) > 100);

        // Four drained and announced, whether or not any grant arrived:
        // four more batches is all the producer can have credit for.
        for item in 4..8u8 {
            sink.r#onBatch(&[item, 0, 0, 0], 1)
                .expect("granted, perhaps");
        }
        assert!(
            sink.r#onBatch(&[8, 0, 0, 0], 1).is_err(),
            "a hundred retries are not a hundred grants"
        );
    }

    /// A poll returns to its caller, who calls again, so a grant that
    /// fails is the next call's to retry: it does not end the stream, and
    /// it is not retried within the call, which would not leave its loop.
    #[test]
    fn a_grant_that_fails_is_retried_by_the_next_poll() {
        let (mut rx, sink, calls) = started::<i32>(2, 4, 1);
        calls.refuse.store(2, Ordering::SeqCst);
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");

        // The producer has now used all it is known to have — the case
        // where a blocking consumer would have to give up.
        assert_eq!(rx.try_recv().expect("no error"), Some(1));
        assert_eq!(calls.attempts.load(Ordering::SeqCst), 1, "refused once");

        assert_eq!(rx.try_recv().expect("the stream goes on"), None);
        assert_eq!(calls.attempts.load(Ordering::SeqCst), 2, "refused twice");

        let before = Instant::now();
        calls.refuse.store(usize::MAX, Ordering::SeqCst);
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(200))
                .expect("the stream goes on"),
            None
        );
        assert!(before.elapsed() >= Duration::from_millis(200));
        assert_eq!(
            calls.attempts.load(Ordering::SeqCst),
            3,
            "one try for the whole wait"
        );

        calls.refuse.store(0, Ordering::SeqCst);
        assert_eq!(rx.try_recv().expect("no error"), None);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 1);
        assert!(rx.end_status().is_none());
    }

    /// A consumer about to block has nobody to call it again. Whether a
    /// failed grant is then the end depends on the producer: one with
    /// credit left sends again and that prompts the retry; one without
    /// waits for this grant, and both ends would wait for good.
    #[test]
    fn a_blocking_consumer_gives_up_only_on_a_producer_that_may_be_out_of_credit() {
        // Credit to spare: four declared, one used.
        let (mut rx, sink, calls) = started::<i32>(2, 4, 4);
        calls.refuse.store(usize::MAX, Ordering::SeqCst);
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");
        let stream = rx.stream.clone();
        let (done, watch) = mpsc::channel();
        let consumer = thread::spawn(move || {
            let first = rx.recv();
            let second = rx.recv();
            let _ = done.send((first, second));
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the consumer waits for the next batch"
        );
        assert!(stream.lock().end.is_none(), "and the stream goes on");
        assert!(
            calls.attempts.load(Ordering::SeqCst) <= 2,
            "a grant tried {} times over is a spin",
            calls.attempts.load(Ordering::SeqCst)
        );
        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");
        let (first, second) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the consumer finishes");
        assert_eq!(first.expect("an item").expect("ok"), 1);
        assert!(second.is_none());
        consumer.join().expect("consumer thread");

        // None to spare: one declared, one used.
        let (mut rx, sink, calls) = started::<i32>(2, 4, 1);
        calls.refuse.store(usize::MAX, Ordering::SeqCst);
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let first = rx.recv();
            let second = rx.recv();
            // Sent back alive: dropping the receiver cancels too.
            let _ = done.send((first, second, rx));
        });
        let (first, second, _rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("neither end could otherwise move");
        assert_eq!(first.expect("an item").expect("ok"), 1);
        assert!(second.expect("a report").is_err());
        assert_eq!(calls.canceled.load(Ordering::SeqCst), 1);
    }

    /// What the producer agrees to send and what the consumer agrees to
    /// read are one set: a status accepted here and refused there makes
    /// `end_with` succeed while the consumer's stream ends on `BadValue`.
    #[test]
    fn a_status_the_producer_accepts_is_one_the_consumer_reads() {
        for code in [
            ExceptionCode::None,
            ExceptionCode::Security,
            ExceptionCode::BadParcelable,
            ExceptionCode::IllegalArgument,
            ExceptionCode::NullPointer,
            ExceptionCode::IllegalState,
            ExceptionCode::NetworkMainThread,
            ExceptionCode::UnsupportedOperation,
            ExceptionCode::ServiceSpecific,
            ExceptionCode::Parcelable,
            ExceptionCode::HasNotedAppOpsReplyHeader,
            ExceptionCode::HasReplyHeader,
            ExceptionCode::TransactionFailed,
            ExceptionCode::JustError,
        ] {
            let Ok((exception, service_specific, message)) = status_fields(&Status::from(code))
            else {
                continue;
            };
            assert!(
                status_from_fields(exception, service_specific, message.as_deref()).is_ok(),
                "{code:?} is sent and cannot be read"
            );
        }
    }

    #[test]
    fn an_exception_code_the_wire_may_not_carry_is_refused() {
        for exception in [-127, -128, -129, 7] {
            assert_eq!(
                status_from_fields(exception, 0, None).err(),
                Some(StatusCode::BadValue),
                "{exception} must not decode into a status"
            );
        }
    }

    /// Every field AOSP's `Status` wire carries survives the three
    /// arguments, which is why they are three rather than one.
    #[test]
    fn a_status_round_trips_through_the_terminator_arguments() {
        for status in [
            Status::from(ExceptionCode::None),
            Status::from((ExceptionCode::Security, "denied")),
            Status::new_service_specific_error(9, Some("busy".to_string())),
        ] {
            let (exception, service_specific, message) =
                status_fields(&status).expect("a carryable status");
            let back = status_from_fields(exception, service_specific, message.as_deref())
                .expect("decode");
            assert_eq!(back, status);
            assert_eq!(back.message(), status.message());
            assert_eq!(
                back.service_specific_error(),
                status.service_specific_error()
            );
        }
    }
}

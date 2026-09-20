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

use std::collections::VecDeque;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicU32, Ordering};
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

    fn grant(&self, credits: u32) {
        {
            let mut state = self.lock();
            state.available = state.available.saturating_add(credits as u64);
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
    fn r#request(&self, credits: i32) -> BinderResult<()> {
        if credits <= 0 {
            // A grant of nothing is a caller mistake, and a negative one
            // would have to mean "take credit back", which the producer
            // may already have spent. The call is `oneway`, so the log
            // is the only place this can be said.
            log::warn!("stream: ignoring a grant of {credits} credits");
            return Ok(());
        }
        self.0.grant(credits as u32);
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

/// A call a `*_async` method handed to the blocking pool, and whether it
/// has left: dropping the future detaches the call rather than cancelling
/// it, so anything sent past it would arrive out of order, and a failure
/// it reported would reach nobody.
#[cfg(feature = "tokio")]
#[derive(Default)]
struct InFlight {
    sent: Mutex<bool>,
    /// The send failed, and how many items went with it — for the next
    /// call to report, since the future that would have is gone.
    lost: Mutex<Option<(StatusCode, i32)>>,
    wake: Condvar,
    notify: tokio::sync::Notify,
}

#[cfg(feature = "tokio")]
impl InFlight {
    fn is_sent(&self) -> bool {
        *self.sent.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn finish(&self) {
        *self.sent.lock().unwrap_or_else(|e| e.into_inner()) = true;
        self.wake.notify_all();
        self.notify.notify_waiters();
    }

    /// Recorded before the guard releases the waiters, so a waiter that
    /// sees the call finished sees this too.
    fn record(&self, e: StatusCode, count: i32) {
        *self.lost.lock().unwrap_or_else(|e| e.into_inner()) = Some((e, count));
    }

    fn take_lost(&self) -> Option<(StatusCode, i32)> {
        self.lost.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    fn wait(&self) {
        let mut sent = self.sent.lock().unwrap_or_else(|e| e.into_inner());
        while !*sent {
            sent = self.wake.wait(sent).unwrap_or_else(|e| e.into_inner());
        }
    }

    async fn wait_async(&self) {
        loop {
            // Created before the check, as in `wait_credit_async`.
            let notified = self.notify.notified();
            if self.is_sent() {
                return;
            }
            notified.await;
        }
    }
}

/// Releases the waiters however the pool task ends — including a task the
/// runtime drops without running it.
#[cfg(feature = "tokio")]
struct InFlightGuard(Arc<InFlight>);

#[cfg(feature = "tokio")]
impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.0.finish();
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
    /// Items that left the pending batch and never reached the consumer.
    /// Latched here because `flush` and `send` return the failure without
    /// a count, so only the terminator can still report them.
    lost: i32,
    /// The batch a dropped `*_async` future left on the blocking pool.
    /// Nothing may be sent until it has gone out.
    #[cfg(feature = "tokio")]
    in_flight: Option<Arc<InFlight>>,
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
    /// a credit round trip at all. The consumer replenishes from there
    /// and need not know this number: grants add to whatever is left.
    ///
    /// `max_batch_bytes` is a threshold, not a hard cap — an item larger
    /// than it still goes out, in a batch of its own. Keep
    /// `initial_credits * max_batch_bytes` within the budget
    /// [`DEFAULT_MAX_BATCH_BYTES`] describes, or the driver starts
    /// counting this stream as a spam suspect.
    ///
    /// # Errors
    ///
    /// Everything [`new`](Self::new) returns, plus
    /// [`StatusCode::BadValue`] for `initial_credits` of zero. The
    /// consumer only grants credit for batches it has taken, so a
    /// producer that starts with none would wait for a grant that
    /// nothing can trigger.
    pub fn with_limits(
        sink: &SIBinder,
        max_batch_bytes: usize,
        initial_credits: u32,
    ) -> Result<Self> {
        if initial_credits == 0 {
            log::error!("Sink::with_limits: the opening window must be at least one batch");
            return Err(StatusCode::BadValue);
        }
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
        if let Err(e) = peer.on_start(&source) {
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
            lost: 0,
            #[cfg(feature = "tokio")]
            in_flight: None,
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
    /// - Anything else is the batch send itself failing — a `oneway` call
    ///   the driver refused, an RPC write that did not go. That batch's
    ///   items are dropped rather than resent, since a `oneway` send that
    ///   reports failure may still have been delivered; the credit comes
    ///   back and the stream stays usable, and [`end`](Self::end) tells
    ///   the consumer how many items went missing.
    pub fn send(&mut self, item: &T) -> Result<()> {
        if self.encode(item)? {
            self.flush()?;
        }
        Ok(())
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
        self.flush_counting().map_err(|(e, _)| e)
    }

    /// [`flush`](Self::flush), reporting with a failure how many items are
    /// still queued and will not go out, so a terminator can add them to
    /// the ones already lost. Items this call itself loses are added to
    /// [`lost`](Self::lost) here, since the callers that drop the count
    /// would otherwise drop them with it.
    fn flush_counting(&mut self) -> std::result::Result<(), (StatusCode, i32)> {
        if self.count == 0 {
            return Ok(());
        }
        // A batch a dropped future left on the pool goes out first, or
        // this one overtakes it; a failure that future could not report
        // is this call's to return, before it sends anything more.
        if let Some((e, lost)) = self.wait_in_flight() {
            self.lost = self.lost.saturating_add(lost);
            return Err((e, 0));
        }
        let queued = self.count;
        self.credit.wait_credit().map_err(|e| (e, queued))?;
        let (bytes, count) = self.take_pending().map_err(|e| {
            // The batch was taken out before the encode of it failed.
            self.lost = self.lost.saturating_add(queued);
            (e, 0)
        })?;
        self.peer.on_batch(&bytes, count).map_err(|e| {
            // The consumer never received this batch, so it will never
            // grant back the credit the batch took; keeping the credit
            // spent would park the producer for a grant nothing can
            // prompt. The batch itself is not restored — a `oneway` send
            // that reports failure may still have been delivered, and
            // resending it would duplicate items.
            self.credit.grant(1);
            self.lost = self.lost.saturating_add(count);
            (e, 0)
        })
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
    /// which a call that arrived cannot claim, and the two reply-header
    /// markers (`-127`, `-128`) mean nothing outside a reply. The sink is
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
        // The terminator must not overtake a batch still on the pool, and
        // it carries what a dropped future's batch lost.
        let stale = self.wait_in_flight().map(|(e, lost)| {
            self.lost = self.lost.saturating_add(lost);
            e
        });
        let failed = match (self.flush_counting(), stale) {
            (Err((e, queued)), _) => {
                // Nothing will send what that flush left queued.
                self.lost = self.lost.saturating_add(queued);
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
        match truncated_terminator(exception, self.lost) {
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
    #[cfg(feature = "tokio")]
    #[must_use = "the item is queued, but a full batch is only sent when this is awaited"]
    pub fn send_async(
        &mut self,
        item: &T,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let encoded = self.encode(item);
        async move {
            if encoded? {
                self.flush_async().await?;
            }
            Ok(())
        }
    }

    /// [`flush`](Self::flush) for a producer running as a task.
    #[cfg(feature = "tokio")]
    pub async fn flush_async(&mut self) -> Result<()> {
        self.flush_counting_async().await.map_err(|(e, _)| e)
    }

    /// [`flush_counting`](Self::flush_counting) for a producer running as
    /// a task.
    #[cfg(feature = "tokio")]
    async fn flush_counting_async(&mut self) -> std::result::Result<(), (StatusCode, i32)> {
        // A batch a dropped future left on the pool goes out first, or
        // this one overtakes it; a failure that future could not report
        // is this call's to return, before it sends anything more.
        if let Some((e, lost)) = self.await_in_flight().await {
            self.lost = self.lost.saturating_add(lost);
            return Err((e, 0));
        }
        if self.count == 0 {
            return Ok(());
        }
        let queued = self.count;
        self.credit
            .wait_credit_async()
            .await
            .map_err(|e| (e, queued))?;
        let (bytes, count) = self.take_pending().map_err(|e| {
            // The batch was taken out before the encode of it failed.
            self.lost = self.lost.saturating_add(queued);
            (e, 0)
        })?;
        let peer = self.peer.clone();
        let credit = self.credit.clone();
        let flight = Arc::new(InFlight::default());
        self.in_flight = Some(flight.clone());
        let guard = InFlightGuard(flight.clone());
        let sent = on_pool(move || {
            let _guard = guard;
            // Both settled where the send ran: dropping this future
            // detaches the pool task rather than cancelling it, so the
            // credit would otherwise be spent with nobody left to notice
            // and the failure would be returned to a future that is gone.
            peer.on_batch(&bytes, count).inspect_err(|e| {
                credit.grant(1);
                flight.record(*e, count);
            })
        })
        .await;
        self.in_flight = None;
        sent.map_err(|e| {
            // This future was here to take the failure, so the record the
            // pool task left on the `InFlight` goes with the last `Arc`
            // and the count is latched once, here.
            self.lost = self.lost.saturating_add(count);
            (e, 0)
        })
    }

    /// [`end`](Self::end) for a producer running as a task.
    ///
    /// Unlike [`send_async`](Self::send_async) and
    /// [`flush_async`](Self::flush_async), this is not a wait to abandon:
    /// the terminator becomes this call's only once it has been handed to
    /// the pool, and a future dropped before that leaves the stream to
    /// [`Drop`](Sink#impl-Drop-for-Sink), which ends it as failed rather
    /// than as done.
    #[cfg(feature = "tokio")]
    pub async fn end_async(self) -> Result<()> {
        self.terminate_async(ExceptionCode::None as i32, 0, None)
            .await
    }

    /// [`end_with`](Self::end_with) for a producer running as a task,
    /// with the same caveat about a dropped future as
    /// [`end_async`](Self::end_async).
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
        // The terminator must not overtake a batch still on the pool, and
        // it carries what a dropped future's batch lost.
        let stale = self.await_in_flight().await.map(|(e, lost)| {
            self.lost = self.lost.saturating_add(lost);
            e
        });
        let failed = match (self.flush_counting_async().await, stale) {
            (Err((e, queued)), _) => {
                // Nothing will send what that flush left queued.
                self.lost = self.lost.saturating_add(queued);
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
            match truncated_terminator(exception, self.lost) {
                Some(truncated) if !canceled => {
                    (ExceptionCode::IllegalState as i32, 0, Some(truncated))
                }
                _ => (exception, service_specific, message),
            };
        let peer = self.peer.clone();
        // Set only now: everything above suspends, and a future dropped
        // there has to leave `Drop` a stream still to terminate. From
        // here there is no suspension point before `on_pool` hands the
        // call to `spawn_blocking`, which a dropped future cannot recall.
        self.ended = true;
        let ended =
            on_pool(move || peer.on_end(exception, service_specific, message.as_deref())).await;
        match failed {
            Some(e) if !canceled => {
                if let Err(end) = ended {
                    log::warn!("stream: the terminator could not be delivered: {end:?}");
                }
                Err(e)
            }
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
    /// taken a credit for it and sends it next.
    fn take_pending(&mut self) -> Result<(Vec<u8>, i32)> {
        let batch = std::mem::replace(&mut self.batch, Parcel::new_data_only());
        let count = std::mem::replace(&mut self.count, 0);
        Ok((batch.into_bytes()?, count))
    }

    /// Block until a batch left on the pool by a dropped future has gone
    /// out, and take the failure that future was not there to report.
    /// Cleared only once the batch has gone, so a second dropped future
    /// waits for the same batch again.
    fn wait_in_flight(&mut self) -> Option<(StatusCode, i32)> {
        #[cfg(feature = "tokio")]
        if let Some(flight) = self.in_flight.clone() {
            flight.wait();
            self.in_flight = None;
            return flight.take_lost();
        }
        None
    }

    /// [`wait_in_flight`](Self::wait_in_flight) for a task.
    #[cfg(feature = "tokio")]
    async fn await_in_flight(&mut self) -> Option<(StatusCode, i32)> {
        if let Some(flight) = self.in_flight.clone() {
            flight.wait_async().await;
            self.in_flight = None;
            return flight.take_lost();
        }
        None
    }
}

/// Run one binder call on the blocking pool and await its result.
/// Through [`Tokio`](crate::Tokio) rather than `spawn_blocking` directly,
/// because inside a transaction handler the call has to stay on the
/// current thread and that rule lives there.
#[cfg(feature = "tokio")]
async fn on_pool<F>(call: F) -> Result<()>
where
    F: FnOnce() -> Result<()> + Send + 'static,
{
    <crate::Tokio as crate::BinderAsyncPool>::spawn(call, |result| async move { result }).await
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
        // The terminator must not overtake a batch still on the pool, and
        // it carries what that batch lost if its send failed.
        let stale = self.wait_in_flight().map_or(0, |(_, lost)| lost);
        // Everything an earlier send already lost is reported here too:
        // nothing after this terminator can.
        let carried = self.lost.saturating_add(stale);
        let mut lost = self.count.saturating_add(carried);
        if self.count > 0 && self.credit.try_credit() {
            let sent = self
                .take_pending()
                .and_then(|(bytes, count)| self.peer.on_batch(&bytes, count));
            match sent {
                // Counted as lost again: `take_pending` emptied the batch
                // before the send that failed.
                Err(e) => log::warn!("stream: the last batch could not be delivered: {e:?}"),
                Ok(()) => lost = carried,
            }
        }
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
    fn on_start(&self, source: &SIBinder) -> Result<()> {
        match self {
            Peer::Typed(sink) => sink.r#onStart(source).map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data = proxy.build_request(<BpStreamSink as crate::Proxy>::descriptor())?;
                data.write(source)?;
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
        ExceptionCode::TransactionFailed
        | ExceptionCode::HasNotedAppOpsReplyHeader
        | ExceptionCode::HasReplyHeader => {
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
}

#[derive(Default)]
struct Stream {
    state: Mutex<StreamState>,
    arrived: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Stream {
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

    /// End the stream with `status` even if something already has — for a
    /// failure the consumer has been handed, which outranks a producer's
    /// terminator that arrived first and would have
    /// [`Receiver::end_status`] report success.
    fn force_end(&self, status: Status) {
        self.lock().end = Some(status);
        self.wake();
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
    fn r#onStart(&self, source: &SIBinder) -> BinderResult<()> {
        let peer = match resolve_peer::<dyn IStreamSource>(
            source,
            <BpStreamSource as crate::Proxy>::descriptor(),
            "IStreamSink.onStart",
        ) {
            Ok(peer) => Arc::new(peer),
            Err(code) => {
                // Without a usable source there is no way to grant, so
                // the stream would stall after the opening window. Say
                // so now; the call is `oneway`, so the consumer is the
                // only one who can be told.
                self.0.end_with(Status::from(code));
                return Ok(());
            }
        };
        // Linked before taking the lock: no binder call under it.
        let death = match watch_death(source, SourceDeath(Arc::downgrade(&self.0))) {
            Ok(death) => death,
            Err(code) => {
                // Unwatched, a producer that dies leaves the consumer
                // blocked for a batch that cannot come. The producer is
                // told too: without this it spends its opening window on
                // batches this stream now discards and then parks on
                // credit no drain will ever prompt.
                let _ = peer.cancel();
                self.0.end_with(Status::from(code));
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
                }
                cancel_now = state.canceled;
            }
        }
        if kept {
            // A consumer that drained its batches before the source
            // existed is parked on credit it had nowhere to grant to;
            // send it back through `advance` now that the grant has
            // somewhere to go.
            self.0.wake();
        } else {
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
            self.0.end_with(status.clone());
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
            state.batches.push_back((items.to_vec(), count));
        }
        self.0.wake();
        Ok(())
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
    /// Batches taken from the queue but not yet paid for with a grant.
    /// Shared with the pool task an async grant runs on, which spends it
    /// there — see [`replenish_async`](Self::replenish_async).
    unacked: Arc<AtomicU32>,
    /// Items decoded from the batch being drained.
    decoded: VecDeque<T>,
    /// Set once the terminator or a decode failure has been reported, so
    /// the error is yielded once and the stream then reads as finished.
    finished: bool,
}

impl<T: Deserialize> Receiver<T> {
    /// A receiver and the sink binder to hand to the producer.
    ///
    /// Pass the binder to the method that starts the stream — declared in
    /// `.aidl` as `rsbinder.stream.IStreamSink` or as a bare `IBinder`.
    pub fn new() -> (Self, SIBinder) {
        Self::with_credit_window(DEFAULT_CREDIT_WINDOW)
    }

    /// [`new`](Self::new) with the grant window set explicitly.
    ///
    /// This is how many drained batches the receiver lets go unpaid
    /// before it grants: a grant leaves once half a window is owed, and
    /// whatever is owed leaves unconditionally once the receiver has
    /// nothing left to drain. A larger window means fewer grant
    /// transactions and a longer wait before the producer sees one.
    ///
    /// It does not set how many batches the producer may have in flight.
    /// That is the producer's opening window — `initial_credits` in
    /// [`Sink::with_limits`], which the consumer never learns — and it
    /// stays the ceiling, because a grant only ever pays for a batch
    /// already drained. See [`DEFAULT_MAX_BATCH_BYTES`] for the budget
    /// that ceiling has to stay inside.
    pub fn with_credit_window(window: u32) -> (Self, SIBinder) {
        let stream = Arc::new(Stream::default());
        let sink_binder = BnStreamSink::new_binder(SinkObject(stream.clone())).as_binder();
        let receiver = Receiver {
            stream,
            sink_binder: sink_binder.clone(),
            window: window.max(1),
            unacked: Arc::new(AtomicU32::new(0)),
            decoded: VecDeque::new(),
            finished: false,
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
    pub fn recv(&mut self) -> Option<BinderResult<T>> {
        loop {
            if let Some(done) = self.advance() {
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
    pub fn try_recv(&mut self) -> BinderResult<Option<T>> {
        match self.advance() {
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
    /// than `timeout` by however long a grant takes.
    pub fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        // A `timeout` no `Instant` can express waits without a bound
        // rather than panicking in `Add`.
        let deadline = Instant::now().checked_add(timeout);
        loop {
            if let Some(done) = self.advance() {
                return done.transpose();
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
    /// the pool is spent there, so it is neither lost nor sent twice.
    ///
    /// # Panics
    ///
    /// The grant reaches `tokio::task::spawn_blocking`, which panics when
    /// there is no Tokio runtime context, so poll this inside one —
    /// [`Sink::send_async`] has the same requirement. A call from inside
    /// a transaction handler runs the grant on the calling thread and is
    /// exempt. Nothing grants before a batch has been drained, so a
    /// consumer polled outside a runtime fails on a later call rather
    /// than the first.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Option<BinderResult<T>> {
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
    /// prompt a later grant.
    fn advance(&mut self) -> Option<Option<BinderResult<T>>> {
        loop {
            if let Some(answer) = self.ready() {
                return Some(answer);
            }
            match self.poll_queue() {
                Queued::Batch(batch) => match self.take_batch(batch) {
                    Ok(()) => self.replenish(false),
                    Err(e) => return Some(self.batch_failed(e)),
                },
                Queued::End => return Some(self.finish()),
                Queued::Nothing => {
                    if self.unacked.load(Ordering::Relaxed) == 0 {
                        return None;
                    }
                    self.replenish(true);
                    // A failed grant ends the stream; look again rather
                    // than go and wait for a batch that cannot come.
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
                    Ok(()) => self.replenish_async(false).await,
                    Err(e) => return Some(self.batch_failed(e)),
                },
                Queued::End => return Some(self.finish()),
                Queued::Nothing => {
                    if self.unacked.load(Ordering::Relaxed) == 0 {
                        return None;
                    }
                    self.replenish_async(true).await;
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
    fn batch_failed(&mut self, e: StatusCode) -> Option<BinderResult<T>> {
        self.finished = true;
        // Forced, not deferred to whatever ended the stream first: the
        // producer's terminator may already be sitting there, and the
        // consumer is being handed this error right now.
        self.stream.force_end(Status::from(e));
        let _ = self.cancel();
        Some(Err(e.into()))
    }

    /// Whether a consumer with nothing to hand out has to park. An empty
    /// queue and no end is not enough: credit owed to a source that only
    /// arrived after the last drain is a grant [`advance`](Self::advance)
    /// can now send, and the producer is parked waiting for exactly that
    /// — so go back through `advance` instead of waiting for a batch it
    /// has no credit to send.
    fn must_wait(&self, state: &StreamState) -> bool {
        if !state.batches.is_empty() || state.end.is_some() {
            return false;
        }
        self.unacked.load(Ordering::Relaxed) == 0 || state.source.is_none()
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
                self.unacked.fetch_add(1, Ordering::Relaxed);
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

    /// Give the producer credit for the batches taken since the last
    /// grant. With more batches already queued (`idle` false) it waits
    /// until half a window is owed; with nothing queued it grants now, or
    /// a producer whose opening window is below that half stalls for good
    /// — out of credit, so no further batch arrives to cross the
    /// threshold.
    fn replenish(&mut self, idle: bool) {
        let Some((source, credits)) = self.grant_due(idle) else {
            return;
        };
        self.unacked.fetch_sub(credits, Ordering::Relaxed);
        let sent = source.request(credits as i32);
        Self::grant_sent(&self.stream, &self.unacked, &source, idle, credits, sent);
    }

    /// [`replenish`](Self::replenish) from a task: on the RPC stack the
    /// send waits for a free outgoing connection, so it does not run on
    /// the executor thread.
    #[cfg(feature = "tokio")]
    async fn replenish_async(&mut self, idle: bool) {
        let Some((source, credits)) = self.grant_due(idle) else {
            return;
        };
        self.unacked.fetch_sub(credits, Ordering::Relaxed);
        let stream = self.stream.clone();
        let unacked = self.unacked.clone();
        // Sent and settled in one pool task. Dropping this future only
        // detaches the task, so a grant that left with the outcome
        // handled back here would be paid for twice: the credit is spent
        // before the send and given back only by the task that failed.
        let _ = on_pool(move || {
            let sent = source.request(credits as i32);
            Self::grant_sent(&stream, &unacked, &source, idle, credits, sent);
            Ok(())
        })
        .await;
    }

    /// The grant that is due now, if one is.
    fn grant_due(&self, idle: bool) -> Option<(Arc<Peer<dyn IStreamSource>>, u32)> {
        let threshold = (self.window / 2).max(1);
        let unacked = self.unacked.load(Ordering::Relaxed);
        if unacked == 0 || (!idle && unacked < threshold) {
            return None;
        }
        // Cloned out so the lock is not held across the binder call. No
        // source yet means a producer that has not sent `onStart`; the
        // credit stays owed until it does.
        let source = self.stream.lock().source.clone()?;
        Some((source, unacked))
    }

    /// A grant that failed keeps its credit for the next attempt; with no
    /// next attempt coming (`idle` — the consumer is about to wait) the
    /// failure ends the stream and stops the producer instead, since both
    /// ends would otherwise wait on each other. The caller has already
    /// spent the credit, so this is where a failed grant gives it back.
    fn grant_sent(
        stream: &Stream,
        unacked: &AtomicU32,
        source: &Peer<dyn IStreamSource>,
        idle: bool,
        credits: u32,
        sent: Result<()>,
    ) {
        match sent {
            Ok(()) => {}
            Err(e) if idle => {
                log::warn!("stream: granting {credits} credits failed: {e:?}");
                stream.end_with(Status::from(e));
                // This stream is over on the consumer's side; a producer
                // parked with no credit has no other way to hear of it.
                let _ = source.cancel();
            }
            Err(e) => {
                log::warn!("stream: granting {credits} credits failed, will retry: {e:?}");
                unacked.fetch_add(credits, Ordering::Relaxed);
            }
        }
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
    fn request(&self, credits: i32) -> Result<()> {
        match self {
            Peer::Typed(source) => source.r#request(credits).map_err(StatusCode::from),
            #[cfg(feature = "rpc")]
            Peer::Unstamped(binder) => {
                let proxy = Self::rpc_proxy(binder)?;
                let mut data =
                    proxy.build_request(<BpStreamSource as crate::Proxy>::descriptor())?;
                data.write(&credits)?;
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

    /// A producer and a consumer in one process.
    ///
    /// The sink binder is local, so a call on it dispatches straight into
    /// the object and no transport is involved. What that leaves under
    /// test is this module's own work — batching, credit, decoding and
    /// the terminator — with the wire covered by `tests/stream_rpc.rs`.
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
    ///
    /// The tests above only show that the producer stops somewhere, which
    /// a window of any size satisfies. This one counts: one batch drained
    /// buys exactly one batch more.
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
        #[derive(Default)]
        struct Recorded {
            refuse: AtomicUsize,
            end: Mutex<Option<(i32, Option<String>)>>,
        }
        struct RefusingSink(Arc<Recorded>);
        impl Interface for RefusingSink {}
        impl IStreamSink for RefusingSink {
            fn r#onStart(&self, _source: &SIBinder) -> BinderResult<()> {
                Ok(())
            }
            fn r#onBatch(&self, _items: &[u8], _count: i32) -> BinderResult<()> {
                if self.0.refuse.swap(0, Ordering::SeqCst) > 0 {
                    return Err(Status::from(ExceptionCode::IllegalState));
                }
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
                Ok(())
            }
        }

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
        let (mut rx, sink_binder) = Receiver::<i32>::new();
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");

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
        let (mut rx, sink_binder) = Receiver::<i32>::new();
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");

        assert!(sink.r#onBatch(&[1, 0, 0, 0], -1).is_err());
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

    /// A producer that sends its first batch before it introduces itself
    /// leaves the consumer holding credit with nowhere to grant it. The
    /// `onStart` that follows has to put the parked consumer back through
    /// `advance`: the producer is out of credit, so no further batch will
    /// arrive to wake it.
    #[test]
    fn an_on_start_after_the_first_batch_pays_the_credit_that_is_owed() {
        let credit = Arc::new(Credit::default());
        let source = BnStreamSource::new_binder(SourceObject(credit.clone())).as_binder();
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(2);
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the receiver's own sink");

        // Drained with no source: one batch is owed and cannot be paid.
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");
        assert_eq!(rx.next().expect("a batch").expect("an item"), 1);

        let (done, watch) = mpsc::channel();
        let consumer = thread::spawn(move || {
            let last = rx.recv_timeout(Duration::from_secs(5));
            let _ = done.send(last.expect("no error").is_none());
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(200)).is_err(),
            "nothing else was sent, so the consumer is waiting"
        );

        sink.r#onStart(&source).expect("onStart");
        let mut granted = false;
        for _ in 0..500 {
            if credit.try_credit() {
                granted = true;
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert!(granted, "the owed grant goes out once the source arrives");

        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");
        assert!(
            watch
                .recv_timeout(Duration::from_secs(5))
                .expect("the consumer finishes"),
            "the terminator ends the stream"
        );
        consumer.join().expect("consumer thread");
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

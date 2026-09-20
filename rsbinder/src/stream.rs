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
//! grant costs the consumer one local send, never a wait on the
//! producer's threads.

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
/// unless [`Sink::with_limits`] or [`Receiver::with_credit_window`] says
/// otherwise.
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

    /// Take one credit if one is there, without waiting.
    ///
    /// For [`Sink`]'s `Drop`, which has to be able to give up: a wait
    /// there would hold whatever thread is unwinding for as long as the
    /// consumer takes to grant.
    fn try_credit(&self) -> bool {
        let mut state = self.lock();
        if state.dead || state.canceled || state.available == 0 {
            return false;
        }
        state.available -= 1;
        true
    }

    /// Take one credit, waiting until one is granted.
    ///
    /// This is the back-pressure: a producer ahead of its consumer parks
    /// here instead of filling the driver's asynchronous space. Three
    /// things end the wait: a grant, a cancel
    /// ([`StatusCode::InvalidOperation`]) and the consumer's process
    /// dying ([`StatusCode::DeadObject`]). Without that last one a
    /// producer parked here would never learn that its consumer is gone
    /// — no batch is in flight to fail, and nothing else will arrive.
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

/// Register `recipient` for `binder`'s death, if that is a thing `binder`
/// can do.
///
/// A local object cannot die while the process holding it runs, and its
/// `link_to_death` says so with an error and a log line, so it is not
/// asked. The returned `Arc` is what keeps the link alive — the binder
/// holds only a `Weak`.
fn watch_death<R>(binder: &SIBinder, recipient: R) -> Option<Arc<dyn crate::DeathRecipient>>
where
    R: crate::DeathRecipient + 'static,
{
    binder.as_remote()?;
    let recipient: Arc<dyn crate::DeathRecipient> = Arc::new(recipient);
    match binder.link_to_death(Arc::downgrade(&recipient)) {
        Ok(()) => Some(recipient),
        Err(e) => {
            // Not fatal: the stream still runs, it just cannot report a
            // peer that vanishes while nothing is in flight.
            log::warn!("stream: cannot watch the peer for death: {e:?}");
            None
        }
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

/// A sink or source binder, resolved once.
///
/// Two shapes, because an RPC proxy that came off the wire has no
/// descriptor: the wire carries an address and no interface string. A
/// typed cast would stamp one permanently (`OnceLock`) onto the proxy the
/// per-address cache shares, so a wrong guess would make every later cast
/// of that object fail for the rest of the process's life. Such a proxy
/// is therefore driven by hand, exactly as
/// [`cancel_remote`](crate::cancel::cancel_remote) drives one.
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

/// What the transport under `binder` can do.
///
/// An RPC proxy answers from its session. Anything else is a local object
/// or a kernel proxy, and both can be called at any time — which is the
/// only bit this module asks about.
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
    /// The object the consumer grants credit on. Held here so it lives
    /// exactly as long as the producer does; the consumer got its own
    /// reference in `onStart`.
    _source: SIBinder,
    /// Holds the death link on the sink; the binder keeps only a `Weak`.
    _death: Option<Arc<dyn crate::DeathRecipient>>,
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
    /// - Whatever sending `onStart` fails with — [`StatusCode::DeadObject`]
    ///   for a consumer that is already gone.
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
        let death = watch_death(sink, SinkDeath(credit.clone()));
        // Before anything else can be sent, so the consumer knows where
        // to grant and whom to watch by the time the first batch lands.
        peer.on_start(&source)?;
        Ok(Sink {
            peer: Arc::new(peer),
            credit,
            batch: Parcel::new_data_only(),
            count: 0,
            max_batch_bytes: max_batch_bytes.max(1),
            ended: false,
            _source: source,
            _death: death,
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
        if self.count == 0 {
            return Ok(());
        }
        self.credit.wait_credit()?;
        let (bytes, count) = self.take_pending()?;
        self.peer.on_batch(&bytes, count)
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
    /// markers (`-127`, `-128`) mean nothing outside a reply.
    pub fn end_with(self, status: &Status) -> Result<()> {
        let (exception, service_specific, message) = status_fields(status)?;
        self.terminate(exception, service_specific, message.as_deref())
    }

    fn terminate(
        mut self,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        if let Err(e) = self.flush() {
            // A cancel is the expected way for that flush to fail, and
            // the terminator still has to go out. Anything else — a dead
            // consumer above all — leaves nothing to tell.
            if !self.credit.is_canceled() {
                // `Drop` runs next and must not send a second
                // terminator: the stream is over either way, and the
                // failure the caller is about to see is the report.
                self.ended = true;
                return Err(e);
            }
        }
        self.ended = true;
        self.peer.on_end(exception, service_specific, message)
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
        if self.count == 0 {
            return Ok(());
        }
        self.credit.wait_credit_async().await?;
        let (bytes, count) = self.take_pending()?;
        let peer = self.peer.clone();
        on_pool(move || peer.on_batch(&bytes, count)).await
    }

    /// [`end`](Self::end) for a producer running as a task.
    #[cfg(feature = "tokio")]
    pub async fn end_async(self) -> Result<()> {
        self.terminate_async(ExceptionCode::None as i32, 0, None)
            .await
    }

    /// [`end_with`](Self::end_with) for a producer running as a task.
    #[cfg(feature = "tokio")]
    pub async fn end_with_async(self, status: &Status) -> Result<()> {
        let (exception, service_specific, message) = status_fields(status)?;
        self.terminate_async(exception, service_specific, message)
            .await
    }

    #[cfg(feature = "tokio")]
    async fn terminate_async(
        mut self,
        exception: i32,
        service_specific: i32,
        message: Option<String>,
    ) -> Result<()> {
        // Same rule as `terminate`: only a cancel lets the terminator go
        // out after a failed flush.
        if let Err(e) = self.flush_async().await {
            if !self.credit.is_canceled() {
                self.ended = true;
                return Err(e);
            }
        }
        self.ended = true;
        let peer = self.peer.clone();
        on_pool(move || peer.on_end(exception, service_specific, message.as_deref())).await
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

    /// Cut the pending batch back to `len` bytes.
    ///
    /// `Parcel` has no truncate — its write cursor may sit anywhere, so
    /// moving it back would not shorten the buffer — hence the rebuild.
    /// Only a failed encode reaches this, so the copy is off the hot
    /// path.
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
}

/// Run one binder call on the blocking pool and await its result.
///
/// Through [`Tokio`](crate::Tokio) rather than `spawn_blocking` directly:
/// inside a transaction handler the call has to stay on the current
/// thread — the kernel's deadlock avoidance and the RPC stack's
/// connection pin both depend on it — and that rule lives there.
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
    /// terminator's message says how many.
    fn drop(&mut self) {
        if self.ended {
            return;
        }
        if self.count > 0 && self.credit.try_credit() {
            let sent = self
                .take_pending()
                .and_then(|(bytes, count)| self.peer.on_batch(&bytes, count));
            if let Err(e) = sent {
                log::warn!("stream: the last batch could not be delivered: {e:?}");
            }
        }
        let message = format!(
            "the stream's producer dropped its sink without ending the stream; \
             {} queued item(s) were not sent",
            self.count
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

/// Rebuild a [`Status`] from the three `onEnd` arguments.
///
/// `service_specific` is read only for `EX_SERVICE_SPECIFIC`, matching
/// AOSP's `Status::writeToParcel`, which writes that field for no other
/// exception.
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

/// What the sink object receives, and what the consumer has asked for.
///
/// Shared between the binder threads that deliver the producer's calls
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
}

/// Watches the producer's source so a consumer blocked in
/// [`Receiver::recv`] learns that no batch is coming.
///
/// It ends the stream the same way a terminator does, with
/// [`StatusCode::DeadObject`] as the reason, so every consuming path
/// reports it without a case of its own. `Weak`, because the stream
/// state owns this recipient.
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
        let death = watch_death(source, SourceDeath(Arc::downgrade(&self.0)));
        let cancel_now = {
            let mut state = self.0.lock();
            if state.source.is_some() {
                log::warn!("stream: ignoring a second onStart");
                return Ok(());
            }
            if !state.closed {
                state.source = Some(peer.clone());
                state.death = death;
            }
            state.canceled
        };
        if cancel_now {
            // The consumer gave up before the producer got this far.
            let _ = peer.cancel();
        }
        Ok(())
    }

    fn r#onBatch(&self, items: &[u8], count: i32) -> BinderResult<()> {
        if count < 0 {
            return Err(Status::from((
                ExceptionCode::IllegalArgument,
                "IStreamSink.onBatch needs a non-negative item count",
            )));
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
    unacked: u32,
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

    /// [`new`](Self::new) with the number of batches kept in flight set
    /// explicitly.
    ///
    /// The receiver grants credit back as it drains, keeping roughly this
    /// many batches outstanding. A larger window costs memory on both
    /// sides and asynchronous space in the producer's driver mapping; see
    /// [`DEFAULT_MAX_BATCH_BYTES`] for what that budget is.
    pub fn with_credit_window(window: u32) -> (Self, SIBinder) {
        let stream = Arc::new(Stream::default());
        let sink_binder = BnStreamSink::new_binder(SinkObject(stream.clone())).as_binder();
        let receiver = Receiver {
            stream,
            sink_binder: sink_binder.clone(),
            window: window.max(1),
            unacked: 0,
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
            while state.batches.is_empty() && state.end.is_none() {
                state = self
                    .stream
                    .arrived
                    .wait(state)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
    }

    /// The next item if one is already here, without blocking.
    ///
    /// `Ok(None)` means nothing has arrived yet and the stream is still
    /// running; the stream being over reads as `Ok(None)` too, which
    /// [`is_finished`](Self::is_finished) tells apart.
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
    pub fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(done) = self.advance() {
                return done.transpose();
            }
            let mut state = self.stream.lock();
            while state.batches.is_empty() && state.end.is_none() {
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
    /// Nothing in here waits on the producer. Granting credit is a
    /// `oneway` call — one local send, returning before the producer has
    /// run — so it is made inline rather than moved to the blocking pool,
    /// where the hand-off would cost more than the call. Cancel-safe: a
    /// future dropped mid-wait loses no item and no credit.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        loop {
            // Created before looking: a `Notified` receives
            // `notify_waiters` from the moment it exists, so a batch
            // landing between the look and the await is not missed.
            // Through a clone of the handle so the waiter does not borrow
            // `self` across `advance`.
            let stream = self.stream.clone();
            let notified = stream.notify.notified();
            if let Some(done) = self.advance() {
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

    /// Everything the consuming paths share: hand out an item, or decode
    /// the next batch, or report the end.
    ///
    /// `Some` is the caller's answer. `None` means nothing is queued and
    /// the stream is still running, so the caller waits in whatever way
    /// it waits — and this has already granted whatever credit was owed,
    /// because a consumer about to wait is the last chance to: a
    /// producer out of credit sends nothing that would prompt a later
    /// grant.
    fn advance(&mut self) -> Option<Option<BinderResult<T>>> {
        loop {
            if let Some(item) = self.decoded.pop_front() {
                return Some(Some(Ok(item)));
            }
            if self.finished {
                return Some(None);
            }
            match self.poll_queue() {
                Queued::Batch(batch) => {
                    if let Err(e) = self.take_batch(batch) {
                        self.finished = true;
                        return Some(Some(Err(e.into())));
                    }
                }
                Queued::End => return Some(self.finish()),
                Queued::Nothing => {
                    if self.unacked == 0 {
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

    fn poll_queue(&self) -> Queued {
        let mut state = self.stream.lock();
        match state.batches.pop_front() {
            Some(batch) => Queued::Batch(batch),
            None if state.end.is_some() => Queued::End,
            None => Queued::Nothing,
        }
    }

    /// Decode a batch and grant credit for having taken it.
    fn take_batch(&mut self, (bytes, count): (Vec<u8>, i32)) -> Result<()> {
        match self.decode_batch(&bytes, count) {
            Ok(()) => {
                self.unacked += 1;
                self.replenish(false);
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
    /// grant.
    ///
    /// With more batches already queued (`idle` false) the grant waits
    /// until half a window is owed: every grant is a transaction, and
    /// half keeps the producer supplied while the consumer drains the
    /// other half. With nothing queued (`idle` true) whatever is owed
    /// goes now. Holding it back then is how a producer whose opening
    /// window is smaller than that half would stall for good — it is out
    /// of credit, so no further batch arrives to push the count over the
    /// threshold.
    ///
    /// A grant that fails to send keeps its credit for the next attempt.
    /// When there will be no next attempt — the consumer is about to
    /// wait — the failure ends the stream instead, since both ends would
    /// otherwise wait on each other.
    fn replenish(&mut self, idle: bool) {
        let threshold = (self.window / 2).max(1);
        if self.unacked == 0 || (!idle && self.unacked < threshold) {
            return;
        }
        // Cloned out so the lock is not held across the binder call. No
        // source yet means a producer that has not sent `onStart`; the
        // credit stays owed until it does.
        let Some(source) = self.stream.lock().source.clone() else {
            return;
        };
        match source.request(self.unacked as i32) {
            Ok(()) => self.unacked = 0,
            Err(e) if idle => {
                log::warn!("stream: granting {} credits failed: {e:?}", self.unacked);
                self.stream.end_with(Status::from(e));
            }
            Err(e) => {
                log::warn!(
                    "stream: granting {} credits failed, will retry: {e:?}",
                    self.unacked
                );
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
        let source = {
            let mut state = self.stream.lock();
            state.canceled = true;
            state.closed = true;
            state.batches.clear();
            // Dropping the recipient is what lets the link lapse; the
            // binder only ever held a `Weak`.
            state.death = None;
            state.source.take()
        };
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

    /// `EX_TRANSACTION_FAILED` says the binder layer failed, which a call
    /// that arrived cannot report, so it is refused before anything is
    /// sent.
    #[test]
    fn a_terminator_that_cannot_be_carried_is_refused() {
        let (sink, _rx) = pair::<i32>(
            DEFAULT_CREDIT_WINDOW,
            DEFAULT_MAX_BATCH_BYTES,
            DEFAULT_CREDIT_WINDOW,
        );
        assert_eq!(
            sink.end_with(&Status::from(ExceptionCode::TransactionFailed))
                .err(),
            Some(StatusCode::BadValue)
        );
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

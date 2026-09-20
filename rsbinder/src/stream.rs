// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Streaming a sequence of values over binder, with back-pressure.
//!
//! A binder method returns one value. A method that would return a
//! million rows either builds the whole list in memory on both sides or
//! invents its own paging protocol. This module is the third option: the
//! consumer hands the producer a **sink** to push batches into, and gets
//! back a **source** it grants credit on, so the producer can only run as
//! far ahead as the consumer allows.
//!
//! The contract is two ordinary `.aidl` interfaces,
//! `rsbinder.stream.IStreamSink` and `rsbinder.stream.IStreamSource`
//! (`rsbinder/aidl/stream/`), so a C++ or Java peer can be either end
//! without rsbinder on its side.
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::stream::Sink;
//! # #[derive(Default)] struct Row;
//! # impl Serialize for Row { fn serialize(&self, _p: &mut Parcel) -> Result<()> { Ok(()) } }
//! # fn rows() -> Vec<Row> { Vec::new() }
//! # fn in_a_handler(sink_binder: &SIBinder) -> Result<SIBinder> {
//! // Service: called with the consumer's sink, hands back the source.
//! let (mut sink, source) = Sink::<Row>::new(sink_binder)?;
//! let handle = source.as_binder();
//! std::thread::spawn(move || {
//!     for row in rows() {
//!         // Blocks here once the granted window is used up.
//!         if sink.send(&row).is_err() {
//!             return;
//!         }
//!     }
//!     let _ = sink.end();
//! });
//! Ok(handle)
//! # }
//! ```
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::stream::Receiver;
//! # #[derive(Default)] struct Row;
//! # impl Deserialize for Row { fn deserialize(_p: &mut Parcel) -> Result<Self> { Ok(Row) } }
//! # fn subscribe(_sink: &SIBinder) -> Result<SIBinder> { unimplemented!() }
//! # fn consume() -> Result<()> {
//! // Consumer: make the receiver, pass its sink, attach the source.
//! let (mut rx, sink_binder) = Receiver::<Row>::new();
//! let source = subscribe(&sink_binder)?;
//! rx.attach_source(&source)?;
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
//! rather than waiting for a batch. The consumer's half needs
//! [`Receiver::attach_source`] — before that it holds no remote handle to
//! watch.
//!
//! **Ordering.** `onBatch` and `onEnd` are both `oneway` to the same
//! object, and the kernel keeps `oneway` calls to one node in order, so
//! the terminator cannot overtake the batches. The credit call runs the
//! other way and is `twoway`, which is how a grant also reports that the
//! consumer is still alive.

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
// here is `Sink`, `Receiver` and `Source`, all of which speak `SIBinder`.
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

/// What the producer waits on and the [`Source`]'s binder feeds.
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
        self.wake.notify_all();
    }

    fn cancel(&self) {
        self.lock().canceled = true;
        self.wake.notify_all();
    }

    fn mark_dead(&self) {
        self.lock().dead = true;
        self.wake.notify_all();
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
            if state.dead {
                return Err(StatusCode::DeadObject);
            }
            if state.canceled {
                return Err(StatusCode::InvalidOperation);
            }
            if state.available > 0 {
                state.available -= 1;
                return Ok(());
            }
            state = self.wake.wait(state).unwrap_or_else(|e| e.into_inner());
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
            // may already have spent.
            return Err(Status::from((
                ExceptionCode::IllegalArgument,
                "IStreamSource.request needs a positive credit count",
            )));
        }
        self.0.grant(credits as u32);
        Ok(())
    }

    fn r#cancel(&self) -> BinderResult<()> {
        self.0.cancel();
        Ok(())
    }
}

/// The producer end as the consumer sees it: the binder to hand back,
/// and what it reports.
///
/// Created together with a [`Sink`] by [`Sink::new`]. Keep it alive for
/// as long as the stream runs — dropping it does not stop the stream,
/// but the consumer's grants land nowhere once its binder is gone.
pub struct Source {
    credit: Arc<Credit>,
    binder: SIBinder,
}

impl Source {
    /// The binder to return from the method that started the stream.
    ///
    /// Declare it in `.aidl` as `rsbinder.stream.IStreamSource`, or as a
    /// bare `IBinder`; the consumer passes it to
    /// [`Receiver::attach_source`] either way.
    pub fn as_binder(&self) -> SIBinder {
        self.binder.clone()
    }

    /// Batches the producer may still send without waiting.
    pub fn credits(&self) -> u64 {
        self.credit.lock().available
    }

    /// Whether the consumer has asked for no more items.
    pub fn is_canceled(&self) -> bool {
        self.credit.is_canceled()
    }
}

impl std::fmt::Debug for Source {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Source")
            .field("credits", &self.credits())
            .field("canceled", &self.is_canceled())
            .finish()
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
/// Made by [`Sink::new`] from the sink binder the consumer supplied,
/// together with the [`Source`] to hand back. Values of `T` are encoded
/// with the same codec as [`crate::to_bytes`], so the consumer's `T` need
/// only agree on the wire, not be the same Rust type.
///
/// Every method that produces takes `&mut self`, so one thread at a time
/// sends and the pending batch needs no lock of its own. Moving the sink
/// to a worker thread after construction is the usual shape — see the
/// [module docs](self).
///
/// Call [`end`](Self::end) on every path, including the failing ones.
/// A `Sink` dropped without it still flushes what it can and still
/// terminates the stream — its `Drop` does, and documents the limits —
/// but the stream is reported as having ended badly, because from the
/// consumer's side that is all a vanished producer can mean.
pub struct Sink<T: ?Sized> {
    peer: Peer<dyn IStreamSink>,
    credit: Arc<Credit>,
    /// Items encoded so far, awaiting a batch send.
    batch: Parcel,
    count: i32,
    max_batch_bytes: usize,
    /// Set once a terminator has gone out, so `Drop` does not send a
    /// second one.
    ended: bool,
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
    pub fn new(sink: &SIBinder) -> Result<(Self, Source)> {
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
    ) -> Result<(Self, Source)> {
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
        let source_binder = BnStreamSource::new_binder(SourceObject(credit.clone())).as_binder();
        let death = watch_death(sink, SinkDeath(credit.clone()));
        let sink = Sink {
            peer,
            credit: credit.clone(),
            batch: Parcel::new_data_only(),
            count: 0,
            max_batch_bytes: max_batch_bytes.max(1),
            ended: false,
            _death: death,
            _item: PhantomData,
        };
        Ok((
            sink,
            Source {
                credit,
                binder: source_binder,
            },
        ))
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
        let mark = self.batch.data_size();
        if let Err(e) = self.batch.write(item) {
            // A failed encode may have written part of the item. Cut the
            // batch back to what the last successful `send` left, so the
            // items already accepted are still delivered intact.
            self.truncate_batch(mark)?;
            return Err(e);
        }
        self.count += 1;
        if self.batch.data_size() >= self.max_batch_bytes {
            self.flush()?;
        }
        Ok(())
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
        self.send_batch()
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
    /// Hand the pending batch to the consumer. A credit must already
    /// have been taken for it.
    fn send_batch(&mut self) -> Result<()> {
        let batch = std::mem::replace(&mut self.batch, Parcel::new_data_only());
        let count = std::mem::replace(&mut self.count, 0);
        let bytes = batch.into_bytes()?;
        self.peer.on_batch(&bytes, count)
    }
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
            if let Err(e) = self.send_batch() {
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

/// Batches and the terminator, as they arrive.
#[derive(Default)]
struct StreamState {
    /// Undecoded batches: bytes and the item count the producer stated.
    /// Decoding happens on the consumer's thread so a malformed batch
    /// surfaces as its error rather than being dropped on a binder
    /// worker.
    batches: VecDeque<(Vec<u8>, i32)>,
    /// Set once by `onEnd`; `None` while the stream runs.
    end: Option<Status>,
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
}

/// Watches the producer's source so a consumer blocked in
/// [`Receiver::recv`] learns that no batch is coming.
///
/// It ends the stream the same way a terminator does, with
/// [`StatusCode::DeadObject`] as the reason, so every consuming path
/// reports it without a case of its own.
struct SourceDeath(Arc<Stream>);

impl crate::DeathRecipient for SourceDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        {
            let mut state = self.0.lock();
            if state.end.is_some() {
                return;
            }
            state.end = Some(Status::from(StatusCode::DeadObject));
        }
        self.0.wake();
    }
}

/// The binder the consumer hands to the producer.
struct SinkObject(Arc<Stream>);

impl Interface for SinkObject {}

impl IStreamSink for SinkObject {
    fn r#onBatch(&self, items: &[u8], count: i32) -> BinderResult<()> {
        if count < 0 {
            return Err(Status::from((
                ExceptionCode::IllegalArgument,
                "IStreamSink.onBatch needs a non-negative item count",
            )));
        }
        {
            let mut state = self.0.lock();
            // After the terminator nothing is part of the stream. A
            // producer that keeps sending is misbehaving; dropping is
            // quieter than growing a queue nobody drains.
            if state.end.is_some() {
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
        {
            let mut state = self.0.lock();
            if state.end.is_none() {
                state.end = Some(status);
            }
        }
        self.0.wake();
        Ok(())
    }
}

/// The consumer's end of a stream: items come out in the order the
/// producer sent them.
///
/// Made by [`Receiver::new`], which also produces the sink binder to pass
/// to the service. Attach the source the service returns with
/// [`attach_source`](Self::attach_source) — without it the producer stops
/// once its opening window is used up.
///
/// Consume with [`recv`](Self::recv), or by iterating: `Receiver`
/// implements [`Iterator`] over `BinderResult<T>`, ending when the
/// producer's terminator arrives.
pub struct Receiver<T> {
    stream: Arc<Stream>,
    sink_binder: SIBinder,
    source: Option<Peer<dyn IStreamSource>>,
    window: u32,
    /// Batches taken from the queue but not yet paid for with a grant.
    unacked: u32,
    /// Items decoded from the batch being drained.
    decoded: VecDeque<T>,
    /// Set once the terminator or a decode failure has been reported, so
    /// the error is yielded once and the stream then reads as finished.
    finished: bool,
    /// Holds the death link on the source; the binder keeps only a `Weak`.
    _death: Option<Arc<dyn crate::DeathRecipient>>,
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
            source: None,
            window: window.max(1),
            unacked: 0,
            decoded: VecDeque::new(),
            finished: false,
            _death: None,
        };
        (receiver, sink_binder)
    }

    /// The sink binder again, for a caller that did not keep the one
    /// [`new`](Self::new) returned.
    pub fn sink_binder(&self) -> SIBinder {
        self.sink_binder.clone()
    }

    /// Take the source the service returned, so grants can reach the
    /// producer.
    ///
    /// Until this is called the producer runs on its opening window alone
    /// and then stops. Calling it twice replaces the source.
    ///
    /// This is also where the producer's process starts being watched: if
    /// it dies the stream ends with [`StatusCode::DeadObject`] instead of
    /// leaving [`recv`](Self::recv) blocked for a batch that nobody will
    /// send. A receiver with no source attached has no remote handle to
    /// watch and so keeps waiting.
    ///
    /// # Errors
    ///
    /// [`StatusCode::BadType`] when `source` states an interface other
    /// than `rsbinder.stream.IStreamSource`.
    pub fn attach_source(&mut self, source: &SIBinder) -> Result<()> {
        self.source = Some(resolve_peer::<dyn IStreamSource>(
            source,
            <BpStreamSource as crate::Proxy>::descriptor(),
            "Receiver::attach_source",
        )?);
        self._death = watch_death(source, SourceDeath(self.stream.clone()));
        Ok(())
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
            if let Some(item) = self.decoded.pop_front() {
                return Some(Ok(item));
            }
            if self.finished {
                return None;
            }
            let batch = {
                let mut state = self.stream.lock();
                loop {
                    if let Some(batch) = state.batches.pop_front() {
                        break Some(batch);
                    }
                    if state.end.is_some() {
                        break None;
                    }
                    state = self
                        .stream
                        .arrived
                        .wait(state)
                        .unwrap_or_else(|e| e.into_inner());
                }
            };
            match batch {
                Some(batch) => {
                    if let Err(e) = self.take_batch(batch) {
                        self.finished = true;
                        return Some(Err(e.into()));
                    }
                }
                None => return self.finish(),
            }
        }
    }

    /// The next item if one is already here, without blocking.
    ///
    /// `Ok(None)` means nothing has arrived yet and the stream is still
    /// running; the stream being over reads as `Ok(None)` too, which
    /// [`is_finished`](Self::is_finished) tells apart.
    pub fn try_recv(&mut self) -> BinderResult<Option<T>> {
        loop {
            if let Some(item) = self.decoded.pop_front() {
                return Ok(Some(item));
            }
            if self.finished {
                return Ok(None);
            }
            let batch = {
                let mut state = self.stream.lock();
                match state.batches.pop_front() {
                    Some(batch) => Some(batch),
                    None if state.end.is_some() => None,
                    None => return Ok(None),
                }
            };
            match batch {
                Some(batch) => {
                    if let Err(e) = self.take_batch(batch) {
                        self.finished = true;
                        return Err(e.into());
                    }
                }
                None => return self.finish().transpose(),
            }
        }
    }

    /// [`recv`](Self::recv) with a bound on how long it waits.
    ///
    /// `Ok(None)` when `timeout` elapses with the stream still running.
    /// A timeout leaves the stream usable — call again.
    pub fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(item) = self.decoded.pop_front() {
                return Ok(Some(item));
            }
            if self.finished {
                return Ok(None);
            }
            let batch = {
                let mut state = self.stream.lock();
                loop {
                    if let Some(batch) = state.batches.pop_front() {
                        break Some(batch);
                    }
                    if state.end.is_some() {
                        break None;
                    }
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
            };
            match batch {
                Some(batch) => {
                    if let Err(e) = self.take_batch(batch) {
                        self.finished = true;
                        return Err(e.into());
                    }
                }
                None => return self.finish().transpose(),
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
    /// Granting credit back to the producer is a `twoway` binder call,
    /// which is synchronous, so this can block the thread for the
    /// duration of that call once per half window. Run it on a
    /// multi-threaded runtime.
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        loop {
            if let Some(item) = self.decoded.pop_front() {
                return Some(Ok(item));
            }
            if self.finished {
                return None;
            }
            // Register before looking: `notify_waiters` only wakes
            // waiters that already exist, so a batch landing between the
            // check and the await would otherwise be missed. Registered
            // through a clone of the handle so the waiter does not
            // borrow `self` across the decode below.
            let stream = self.stream.clone();
            let notified = stream.notify.notified();
            // Two levels: the outer `None` is "nothing has arrived yet",
            // the inner one is "the terminator has". Resolved inside the
            // block so the guard is gone before the await below.
            let ready = {
                let mut state = stream.lock();
                match state.batches.pop_front() {
                    Some(batch) => Some(Some(batch)),
                    None if state.end.is_some() => Some(None),
                    None => None,
                }
            };
            let Some(batch) = ready else {
                notified.await;
                continue;
            };
            match batch {
                Some(batch) => {
                    if let Err(e) = self.take_batch(batch) {
                        self.finished = true;
                        return Some(Err(e.into()));
                    }
                }
                None => return self.finish(),
            }
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
    /// still send a terminator.
    ///
    /// Sent automatically when a `Receiver` is dropped, so a consumer
    /// that walks away does not leave the producer waiting.
    pub fn cancel(&self) -> Result<()> {
        match &self.source {
            Some(source) => source.cancel(),
            None => Ok(()),
        }
    }

    /// Decode a batch and grant credit for having taken it.
    fn take_batch(&mut self, (bytes, count): (Vec<u8>, i32)) -> Result<()> {
        match self.decode_batch(&bytes, count) {
            Ok(()) => {
                self.unacked += 1;
                self.replenish();
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
    /// grant, once enough have piled up to be worth a round trip.
    ///
    /// Half a window rather than every batch: `request` is a `twoway`
    /// call, and granting one at a time would put a round trip between
    /// every pair of batches. Half keeps the producer supplied while the
    /// consumer is still draining the other half.
    fn replenish(&mut self) {
        let threshold = (self.window / 2).max(1);
        if self.unacked < threshold {
            return;
        }
        let Some(source) = &self.source else {
            return;
        };
        let credits = std::mem::take(&mut self.unacked);
        if let Err(e) = source.request(credits as i32) {
            // The producer is gone or refused the grant. Nothing further
            // will arrive, and `recv` finds that out through the queue
            // rather than here.
            log::warn!("stream: granting {credits} credits failed: {e:?}");
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
        if let Some(source) = &self.source {
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
            .field("attached", &self.source.is_some())
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
                        crate::FLAG_CLEAR_BUF | crate::FLAG_PRIVATE_LOCAL,
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
        let (mut rx, sink_binder) = Receiver::<T>::with_credit_window(window);
        let (sink, source) = Sink::<T>::with_limits(&sink_binder, max_batch_bytes, initial_credits)
            .expect("a local binder can always be called back");
        rx.attach_source(&source.as_binder())
            .expect("attach_source");
        // The receiver's typed handle keeps the source object alive, so
        // the producer's credit state outlives this value.
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
        let (mut sink, source) = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");
        rx.attach_source(&source.as_binder())
            .expect("attach_source");

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
        let (mut sink, source) = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");
        rx.attach_source(&source.as_binder())
            .expect("attach_source");

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

    #[test]
    fn a_cancel_releases_a_producer_waiting_for_credit() {
        let (mut rx, sink_binder) = Receiver::<i32>::with_credit_window(1);
        let (mut sink, source) = Sink::<i32>::with_limits(&sink_binder, 4, 1).expect("with_limits");
        rx.attach_source(&source.as_binder())
            .expect("attach_source");

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
        let (mut sink, source) = Sink::<i32>::with_limits(&sink_binder, 8, 1).expect("with_limits");
        rx.attach_source(&source.as_binder())
            .expect("attach_source");

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

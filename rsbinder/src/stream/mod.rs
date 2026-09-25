// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Streaming a sequence of values over binder, with back-pressure.
//!
//! A binder method returns one value. A method that would return a
//! million rows either builds the whole list in memory on both sides or
//! invents its own paging protocol. This module is the third option: the
//! consumer makes an **endpoint** and hands it to the producer in the one
//! call that opens the stream; the producer writes items into it and can
//! only run as far ahead as the consumer allows.
//!
//! ```no_run
//! # use rsbinder::*;
//! # use rsbinder::stream::{Sink, StreamEndpoint};
//! # #[derive(Default)] struct Row;
//! # impl Serialize for Row { fn serialize(&self, _p: &mut Parcel) -> Result<()> { Ok(()) } }
//! # fn rows() -> Vec<Row> { Vec::new() }
//! # fn in_a_handler(endpoint: &StreamEndpoint) -> Result<()> {
//! // Service: called with the consumer's endpoint. The method itself
//! // has nothing to return — the stream carries everything.
//! let mut sink = Sink::<Row>::open(endpoint)?;
//! std::thread::spawn(move || {
//!     for row in rows() {
//!         // Blocks here once the consumer has fallen behind.
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
//! An async service does the same from a task, where waiting for the
//! consumer suspends the task instead of parking a thread:
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
//! # use rsbinder::stream::{Receiver, StreamEndpoint};
//! # #[derive(Default)] struct Row;
//! # impl Deserialize for Row { fn deserialize(_p: &mut Parcel) -> Result<Self> { Ok(Row) } }
//! # fn subscribe(_service: &SIBinder, _endpoint: &StreamEndpoint) -> Result<()> { unimplemented!() }
//! # fn consume(service: &SIBinder) -> Result<()> {
//! // Consumer: make the receiver against the service, pass its
//! // endpoint, read.
//! let (mut rx, endpoint) = Receiver::<Row>::new(service)?;
//! subscribe(service, &endpoint)?;
//! for row in &mut rx {
//!     let _row = row?;
//! }
//! # Ok(())
//! # }
//! ```
//!
//! # The endpoint, and the call that opens a stream
//!
//! [`StreamEndpoint`] is an ordinary AIDL parcelable,
//! `rsbinder.stream.StreamEndpoint` (`rsbinder/aidl/stream/`), that a
//! service's own `.aidl` takes as an argument or returns. `rsbinder-aidl`
//! resolves an `import rsbinder.stream.StreamEndpoint;` to this type, so
//! no copy of the file is needed. One `twoway` call opens the stream, and
//! there is no second call:
//!
//! | The client is… | The call | Death links |
//! |---|---|---|
//! | the consumer (a download) | `void subscribe(in StreamEndpoint endpoint)` | `Receiver::new(&service)`; the producer links to `endpoint.sink` |
//! | the producer (an upload) | `StreamEndpoint upload(IBinder producer)` | `Receiver::new(&producer)` in the handler; the producer links to `endpoint.sink` |
//!
//! [`Receiver::new`] takes the **peer**: a binder in the producer's
//! process. In a download that is the service about to be called; in an
//! upload it is the binder the client passed — any object of its own, or
//! a [`Token`] made for the purpose. The peer decides two things: which
//! transport the stream runs on, and whom the consumer watches for death.
//! A peer that is not in the producer's process leaves the producer's
//! death unseen.
//!
//! Because the call is `twoway`, a refusal — the transport cannot carry
//! the stream, the ring is larger than the producer accepts, SELinux
//! denies the mapping — comes back to the caller as the call's error. A
//! service that starts a stream from a `oneway` callback has no reply to
//! carry the endpoint, and needs one more call from the consumer.
//!
//! # Two transports
//!
//! **Kernel binder** (Linux and Android, also two ends in one process):
//! the endpoint carries a **ring**, a synchronized Fast Message Queue of
//! bytes ([`crate::fmq`]) that the consumer allocates, sealed and charged
//! to its own process, and the producer maps. Items are written into it
//! as records and read out of it; no binder call carries an item, and
//! after the opening call no call goes from consumer to producer at all.
//! A full ring parks the producer on the ring's futex until the consumer
//! reads; an empty ring parks the consumer until the producer writes.
//! The ring's size is the whole of the flow control
//! ([`ReceiverPolicy::ring_bytes`]), and it bounds the largest item:
//! `ring_bytes - `[`END_RESERVE`]` - 4`. The layout is in
//! `StreamEndpoint.aidl`; a C++ peer using `libfmq` implements against
//! it.
//!
//! **RPC** (a session over a socket): the endpoint carries only the
//! consumer's sink, and the stream is the two `oneway` interfaces
//! `rsbinder.stream.IStreamSink` and `IStreamSource`, credit-based: the
//! producer states an opening window of batches, sends up to it, and the
//! consumer grants more as it drains ([`SinkPolicy::initial_credits`],
//! [`ReceiverPolicy::credit_window`], [`ReceiverPolicy::max_opening`]).
//! The producer has to be able to call the consumer outside a handler,
//! so [`Sink::open`] refuses a session without
//! [`TransportCaps::CALLBACKS`](crate::TransportCaps::CALLBACKS) rather
//! than letting the first batch fail. A peer's death is a session that
//! ends.
//!
//! Which one a stream gets is the consumer's peer: a kernel proxy or a
//! local object makes a ring, an RPC proxy makes a sink-only endpoint.
//! [`Sink::open`] follows whichever the endpoint carries.
//!
//! # What the two have in common
//!
//! * **Items carry no binder and no file descriptor.** An item is bytes
//!   in parcel encoding with no object table, so one holding either is
//!   refused at [`send`](Sink::send) — see [`crate::to_bytes`], which
//!   uses the same parcel mode. The consumer's `T` need only agree on the
//!   wire, not be the same Rust type.
//! * **The stream ends with a status.** [`Sink::end`] ends it clean;
//!   [`Sink::end_with`] carries a `Status`, so a service-specific failure
//!   reaches the consumer with its code and message intact, as the error
//!   [`Receiver::recv`] yields. A `Sink` dropped without either still
//!   ends the stream, as having failed. The end never overtakes an item:
//!   on the ring it is the last record, on RPC the last `oneway` call.
//! * **A peer that dies ends the stream.** Back-pressure means that most
//!   of the time neither side has a call in flight to fail on, so each
//!   watches the other's binder: a producer parked for room reports
//!   [`StatusCode::DeadObject`] from [`Sink::send`], a consumer parked
//!   for an item yields it from [`Receiver::recv`].
//! * **Cancel.** [`Receiver::cancel`], and dropping a `Receiver`, release
//!   a parked producer, whose next [`Sink::send`] reports
//!   [`StatusCode::InvalidOperation`].
//!
//! # Async
//!
//! With the `tokio` feature, every `*_async` method makes its wait from
//! the blocking pool — a futex wait on the ring, a `oneway` send on RPC —
//! so a producer or consumer that has to wait does not hold an executor
//! thread. A futex cannot be polled, so on the ring each wait holds one
//! pool thread for as long as it lasts, the same cost as a thread of its
//! own. **Poll them inside a Tokio runtime**: outside one the hand-off
//! panics, as `tokio::task::spawn_blocking` does. A call from inside a
//! transaction handler makes its wait on the calling thread instead and
//! is exempt.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use crate::binder::Interface;
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, Serialize};
use crate::status::{BinderResult, ExceptionCode, Status};
use crate::SIBinder;

// The generated contract is an implementation detail bar the endpoint:
// the public surface here is `Sink` and `Receiver`. Exposing the tree
// would put every generated trait, proxy and stub under the crate's
// stability promise, and a caller that wants the raw interface can
// compile the shipped `.aidl` itself.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stream.rs"));
}

mod calls;
#[cfg(feature = "tokio")]
mod pool;
mod ring;

pub use generated::rsbinder::stream::StreamEndpoint::StreamEndpoint;

/// Bytes at the end of a ring that item records never occupy, so that the
/// end record — at most this long, its message cut to fit — always has
/// room and [`Sink::end`] never waits for the consumer to make it.
///
/// 256 holds the 4-byte header, the two `int` status fields and a line of
/// message, and is 0.4% of the default ring. A ring must be larger than
/// this, and its largest item is `ring_bytes - END_RESERVE - 4`.
pub const END_RESERVE: usize = 256;

/// What a [`Receiver`] is made with: the ring on the kernel path, the
/// credit window on the RPC path. The peer decides which applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverPolicy {
    /// Kernel binder: bytes of ring the consumer allocates, and so the
    /// most the producer can run ahead by. Must exceed [`END_RESERVE`];
    /// the largest item is `ring_bytes - END_RESERVE - 4`, so a stream of
    /// large items needs a larger ring. The memory is allocated up front
    /// and charged to the consumer's process.
    ///
    /// Default 64 KiB: sixteen pages, the same in-flight bound as four
    /// 16 KiB batches on the RPC path. A producer accepts a ring up to
    /// [`SinkPolicy::max_ring_bytes`].
    pub ring_bytes: usize,
    /// RPC: how many drained batches the consumer lets go unpaid before
    /// it grants. A grant leaves once half this window is owed — or the
    /// producer's whole opening window, if that is less — and whatever is
    /// owed leaves before the consumer waits. A larger window means fewer
    /// grant calls and a longer wait before the producer sees one. Zero
    /// is taken as one.
    ///
    /// It does not set how many batches the producer may have in flight:
    /// that is the producer's opening window, which stays the ceiling
    /// because a grant only pays for a batch already drained.
    pub credit_window: u32,
    /// RPC: the widest opening window this consumer accepts. The producer
    /// states its window when it introduces itself, and the consumer
    /// holds it to that plus what it has granted since — a batch beyond
    /// it ends the stream with `EX_ILLEGAL_STATE` rather than being
    /// queued. That is what bounds the memory an untrusted producer (a
    /// client uploading to a service) can make the consumer hold. A
    /// producer that opens wider is refused at the start, with
    /// `EX_ILLEGAL_ARGUMENT`. Zero is taken as one.
    ///
    /// Default 4, the producer's own default
    /// ([`SinkPolicy::initial_credits`]), so a producer made with
    /// [`Sink::open`] is always taken by a consumer made with
    /// [`Receiver::new`].
    pub max_opening: u32,
}

impl Default for ReceiverPolicy {
    fn default() -> Self {
        ReceiverPolicy {
            ring_bytes: 64 * 1024,
            credit_window: 4,
            max_opening: 4,
        }
    }
}

/// What a [`Sink`] is opened with: what ring it accepts on the kernel
/// path, how it batches on the RPC path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SinkPolicy {
    /// Kernel binder: the largest ring this producer maps. The ring is
    /// the consumer's memory, and a hostile consumer could describe one
    /// of any size; this is the producer's address space at stake.
    ///
    /// Default 4 MiB, the ceiling
    /// [`ProcessState::init_with_mmap_size`](crate::ProcessState::init_with_mmap_size)
    /// puts on the binder mapping itself.
    pub max_ring_bytes: usize,
    /// RPC: the most bytes one batch carries before it is sent. A
    /// threshold, not a cap — an item larger than it still goes out, with
    /// the batch it was added to. Default 16 KiB.
    pub max_batch_bytes: usize,
    /// RPC: batches the producer may send before the consumer grants
    /// anything, so that a short stream finishes without a credit round
    /// trip. Stated to the consumer, which ends the stream at once when
    /// it is more than [`ReceiverPolicy::max_opening`]. At least one: the
    /// consumer only grants for batches it has drained, so a producer
    /// that started with none would wait for a grant nothing can
    /// trigger. Default 4.
    pub initial_credits: u32,
}

impl Default for SinkPolicy {
    fn default() -> Self {
        SinkPolicy {
            max_ring_bytes: 4 * 1024 * 1024,
            max_batch_bytes: 16 * 1024,
            initial_credits: 4,
        }
    }
}

/// A binder for a producer that has none of its own to hand over.
///
/// In an upload the client is the producer and the service the consumer,
/// and the service's [`Receiver::new`] needs a binder in the client's
/// process to watch for death and to pick the transport by. A client with
/// a callback object of its own passes that; one without makes a `Token`,
/// passes [`binder`](Self::binder) as the call's argument, keeps the
/// `Token` for as long as it streams, and lets it drop afterwards. The
/// object answers no transaction.
pub struct Token {
    binder: SIBinder,
}

struct TokenObject;

impl crate::Remotable for TokenObject {
    fn descriptor() -> &'static str {
        "rsbinder.stream.Token"
    }

    fn on_transact(
        &self,
        _code: crate::TransactionCode,
        _reader: &mut Parcel,
        _reply: &mut Parcel,
    ) -> Result<()> {
        Err(StatusCode::UnknownTransaction)
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

impl Token {
    /// A fresh token.
    pub fn new() -> Self {
        Token {
            binder: crate::native::Binder::new(TokenObject).as_binder(),
        }
    }

    /// The binder to pass as the argument of an upload call.
    pub fn binder(&self) -> SIBinder {
        self.binder.clone()
    }
}

impl Default for Token {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Token").finish()
    }
}

// ---------------------------------------------------------------------
// Shared by both paths
// ---------------------------------------------------------------------

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
    // Callers include `Drop`: a kernel proxy's `unlink_to_death` panics on
    // a poisoned recipients lock, and a panic leaving `Drop` during an
    // unwind aborts the process (`bridge::unlink_all` catches for this).
    let unlinked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = binder.unlink_to_death(Arc::downgrade(recipient));
    }));
    if unlinked.is_err() {
        log::error!("stream: unlink_to_death panicked; a death link stays registered");
    }
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

/// Whether a stream to `peer` runs on a ring: the two ends share a kernel
/// binder driver — or a process — and the platform has the shared memory
/// and futex the ring needs. Anything else is the RPC path.
fn over_ring(peer: &SIBinder) -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
        && peer_caps(peer).contains(crate::TransportCaps::KERNEL_KNOBS)
}

/// One item as the bytes a record or a batch carries: the `to_bytes`
/// codec, which refuses a binder or a file descriptor.
fn encode_item<T: Serialize + ?Sized>(item: &T) -> Result<Vec<u8>> {
    let mut parcel = Parcel::new_data_only();
    parcel.write(item)?;
    parcel.into_bytes()
}

/// One item back from its bytes, all of them: bytes left over mean the
/// producer's `T` and this one disagree on the wire.
fn decode_item<T: Deserialize>(bytes: &[u8]) -> Result<T> {
    let mut parcel = Parcel::from_slice(bytes);
    let item = parcel.read::<T>()?;
    if parcel.data_avail() != 0 {
        log::error!(
            "stream: {} bytes left after one item; the two ends disagree on the item type",
            parcel.data_avail()
        );
        return Err(StatusCode::BadValue);
    }
    Ok(item)
}

/// What the consumer is told when [`Sink::end_with`] was handed a status
/// the wire cannot carry. The sink is consumed by then, so the stream ends
/// on this rather than on the producer's own status.
const UNCARRIABLE_TERMINATOR: &str =
    "the stream's producer ended with a status that cannot be carried";

/// The terminator for a stream that lost items: a clean `EX_NONE` over
/// lost items would read exactly like a stream that ran out, so it is
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

/// The terminator's message for a `Sink` dropped without `end`.
fn dropped_terminator(lost: i32) -> String {
    format!(
        "the stream's producer dropped its sink without ending the stream; \
         {lost} queued item(s) were not sent"
    )
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
            log::error!("stream: {other} is not a valid stream terminator exception code");
            return Err(StatusCode::BadValue);
        }
    })
}

// ---------------------------------------------------------------------
// Producer
// ---------------------------------------------------------------------

enum SinkInner<T: ?Sized> {
    /// Boxed: the pending batch and the credit state make it several
    /// times the ring producer's size.
    Calls(Box<calls::Producer<T>>),
    Ring(ring::Producer<T>),
}

/// The producer's handle on a stream: write items in, they reach the
/// consumer in order.
///
/// Made by [`Sink::open`] from the endpoint the consumer supplied. Values
/// of `T` are encoded with the same codec as [`crate::to_bytes`], so the
/// consumer's `T` need only agree on the wire, not be the same Rust type.
///
/// Every method that produces takes `&mut self`, so one thread or task at
/// a time sends. Moving the sink to a worker thread after opening it is
/// the usual shape — see the [module docs](self) — and with the `tokio`
/// feature the `*_async` methods do the same from a task.
///
/// Call [`end`](Self::end) on every path, including the failing ones.
/// A `Sink` dropped without it still terminates the stream, but as having
/// failed, because from the consumer's side that is all a vanished
/// producer can mean.
///
/// # Drop
///
/// A producer that goes away without [`end`](Self::end) leaves the
/// consumer blocked in [`Receiver::recv`] for a terminator that no longer
/// has a sender, and the consumer cannot recover on its own: the
/// producer's process is still running, so no death link fires. Dropping
/// the sink is the fallback for that, not a substitute for `end` — the
/// stream ends with `EX_ILLEGAL_STATE`.
///
/// The drop does not wait for the consumer. Blocking in a `Drop` would
/// hold the unwinding thread for as long as the consumer takes to read,
/// so on the RPC path items that cannot leave without credit are lost,
/// and on the ring a record a dropped `send_async` future left waiting
/// for room is given up; the terminator's message says how many. It does
/// wait for a batch a dropped future left on the pool on the RPC path,
/// which is bounded by that one send rather than by the consumer.
pub struct Sink<T: ?Sized> {
    inner: SinkInner<T>,
}

impl<T: Serialize + ?Sized> Sink<T> {
    /// Take the consumer's endpoint and start a stream, with the default
    /// [`SinkPolicy`].
    ///
    /// # Errors
    ///
    /// - [`StatusCode::BadValue`] for an endpoint with no sink, a ring the
    ///   producer will not map (larger than
    ///   [`SinkPolicy::max_ring_bytes`], not sealed against shrinking,
    ///   no EventFlag word, or a descriptor that does not describe a
    ///   ring), or a ring no larger than [`END_RESERVE`].
    /// - [`StatusCode::InvalidOperation`] when the transport cannot carry
    ///   the stream: on the RPC path, a session whose client did not open
    ///   incoming connections
    ///   ([`ClientOptions::incoming_connections`](crate::ClientOptions::incoming_connections));
    ///   the log line says so. A ring endpoint on a platform without
    ///   kernel binder.
    /// - [`StatusCode::BadType`] on the RPC path when the sink states
    ///   some other interface. A binder with no interface yet — an RPC
    ///   proxy fresh off the wire — cannot be checked and is accepted; a
    ///   wrong one then shows up as the consumer's server refusing the
    ///   interface token.
    /// - Whatever watching the consumer for death, or the RPC path's
    ///   `onStart`, fails with — [`StatusCode::DeadObject`] for a consumer
    ///   that is already gone.
    pub fn open(endpoint: &StreamEndpoint) -> Result<Self> {
        Self::open_with(endpoint, &SinkPolicy::default())
    }

    /// [`open`](Self::open) with the policy set explicitly.
    ///
    /// # Errors
    ///
    /// Everything [`open`](Self::open) returns, plus
    /// [`StatusCode::BadValue`] for an [`initial_credits`](SinkPolicy::initial_credits)
    /// of zero, or too large for the `int` that carries it. A window the
    /// consumer refuses is not an error here — `onStart` is `oneway` —
    /// but a cancel, which [`send`](Self::send) reports when it next has
    /// a batch to send; the reason is on the consumer's side.
    pub fn open_with(endpoint: &StreamEndpoint, policy: &SinkPolicy) -> Result<Self> {
        let Some(sink) = endpoint.sink.as_ref() else {
            log::error!("Sink::open: the endpoint has no sink");
            return Err(StatusCode::BadValue);
        };
        let inner = match &endpoint.ring {
            Some(ring) => SinkInner::Ring(ring::Producer::open(ring, sink, policy)?),
            None => SinkInner::Calls(Box::new(calls::Producer::open(sink, policy)?)),
        };
        Ok(Sink { inner })
    }

    /// Add one item to the stream.
    ///
    /// On the ring the item is written now, as one record, and **this
    /// call blocks** while the ring is full: that is the back-pressure,
    /// and it is why a `Sink` belongs on a thread of its own rather than
    /// on a binder worker. On the RPC path the item is encoded and
    /// queued, and leaves once the pending batch reaches
    /// [`SinkPolicy::max_batch_bytes`] or at [`flush`](Self::flush) or
    /// [`end`](Self::end); there the call blocks when a batch is ready to
    /// go and the consumer has granted no credit for it.
    ///
    /// # Errors
    ///
    /// - [`StatusCode::InvalidOperation`] once the consumer has
    ///   cancelled. Nothing further will be delivered, so stop.
    /// - [`StatusCode::DeadObject`] when the consumer's process is gone.
    /// - [`StatusCode::BadType`] / [`StatusCode::FdsNotAllowed`] for an
    ///   item holding a binder or a file descriptor. Nothing is written;
    ///   items already accepted are unaffected.
    /// - [`StatusCode::BadValue`], on the ring, for an item larger than
    ///   the ring's largest (`ring_bytes - `[`END_RESERVE`]` - 4`); it is
    ///   not written, and the stream goes on. Also for a ring whose
    ///   counters the consumer has moved out of their invariant, after
    ///   which nothing more can be written and every later call returns
    ///   it.
    /// - Anything else is a batch send on the RPC path failing, and that
    ///   batch's items are lost: [`end`](Self::end) tells the consumer
    ///   how many went missing, so the stream ends as failed either way.
    ///   A dead session, a refused transaction
    ///   ([`StatusCode::FailedTransaction`]) or an expired send deadline
    ///   ([`StatusCode::TimedOut`]) mean the batch did not arrive; its
    ///   credit comes back and the stream stays usable. Any other failure
    ///   leaves it unknown whether the batch arrived, and with it how
    ///   much credit is left; the consumer ends a stream on a batch sent
    ///   without credit, so nothing more is sent, and every later
    ///   [`send`](Self::send) and [`flush`](Self::flush) returns the same
    ///   error without queueing anything.
    pub fn send(&mut self, item: &T) -> Result<()> {
        match &mut self.inner {
            SinkInner::Calls(p) => p.send(item),
            SinkInner::Ring(p) => p.send(item),
        }
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

    /// Make sure everything accepted so far is on its way.
    ///
    /// On the ring nothing is queued — every accepted item is already in
    /// the ring — so this returns at once, unless a `send_async` future
    /// was dropped with its record still waiting for room, in which case
    /// it waits for that record. On the RPC path it sends the pending
    /// batch even if it is not full, blocking for credit when there is
    /// something to send; it does nothing, and cannot block, when nothing
    /// is queued. There, batches leave on the byte threshold and on this
    /// call, never on a clock: a producer whose items arrive at their own
    /// pace — an event feed, a log tail — calls this after each event or
    /// each burst, or an item sits in the pending batch until the next
    /// [`send`](Self::send) fills it, which for an idle producer is not a
    /// bounded wait. Flushing after every item costs a producer on the
    /// ring nothing, so a producer unsure which path it is on can simply
    /// flush.
    pub fn flush(&mut self) -> Result<()> {
        match &mut self.inner {
            SinkInner::Calls(p) => p.flush(),
            SinkInner::Ring(p) => p.flush(),
        }
    }

    /// Finish the stream: the items ran out, nothing went wrong.
    ///
    /// On the RPC path queued items are flushed first, and that flush
    /// blocks for credit — unless the consumer has cancelled, in which
    /// case they are dropped and only the terminator goes out. On the
    /// ring the end record has room reserved for it ([`END_RESERVE`]),
    /// so this never waits for the consumer.
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
    /// already succeeded. On the ring the message is cut to what
    /// [`END_RESERVE`] holds, 244 bytes, on a character boundary.
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
    /// consumer could not read either. The sink is consumed either way,
    /// so the stream still ends — with `EX_ILLEGAL_ARGUMENT`, saying the
    /// producer's own status was the thing that could not be sent.
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

    fn terminate(self, exception: i32, service_specific: i32, message: Option<&str>) -> Result<()> {
        match self.inner {
            SinkInner::Calls(p) => p.terminate(exception, service_specific, message),
            SinkInner::Ring(p) => p.terminate(exception, service_specific, message),
        }
    }

    /// [`send`](Self::send) for a producer running as a task.
    ///
    /// Same contract, with the wait moved off the executor thread: on the
    /// ring a record that finds no room is written from the blocking
    /// pool, on the RPC path waiting for credit suspends the task and the
    /// batch is sent from the pool, the way every generated async proxy
    /// sends ([`Tokio`](crate::Tokio)).
    ///
    /// The item is encoded **when this is called**, not when the future
    /// is first polled, so the future does not borrow `item` and stays
    /// `Send` whatever `T` is. Dropping the future unpolled therefore
    /// leaves the item queued (RPC) or unwritten (ring). Dropping it
    /// mid-wait is safe too: a record or batch already handed to the pool
    /// still goes out, in its place, and the next call waits for it
    /// first.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    #[must_use = "the item is only written or sent when this is awaited"]
    pub fn send_async(
        &mut self,
        item: &T,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        match &mut self.inner {
            SinkInner::Calls(p) => pool::Either::A(p.send_async(item)),
            SinkInner::Ring(p) => pool::Either::B(p.send_async(item)),
        }
    }

    /// [`flush`](Self::flush) for a producer running as a task.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    pub async fn flush_async(&mut self) -> Result<()> {
        match &mut self.inner {
            SinkInner::Calls(p) => p.flush_async().await,
            SinkInner::Ring(p) => p.flush_async().await,
        }
    }

    /// [`end`](Self::end) for a producer running as a task.
    ///
    /// Unlike [`send_async`](Self::send_async) and
    /// [`flush_async`](Self::flush_async), this is not a wait to abandon:
    /// the terminator becomes this call's only once nothing of it can
    /// still suspend — on the ring, once the record a dropped future left
    /// has gone in; on RPC, once the terminator has been handed to the
    /// pool — and a future dropped before that leaves the stream to
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
        self,
        exception: i32,
        service_specific: i32,
        message: Option<String>,
    ) -> Result<()> {
        match self.inner {
            SinkInner::Calls(p) => {
                p.terminate_async(exception, service_specific, message)
                    .await
            }
            SinkInner::Ring(p) => {
                p.terminate_async(exception, service_specific, message)
                    .await
            }
        }
    }
}

impl<T: ?Sized> Sink<T> {
    /// Whether the consumer has asked for no more items.
    ///
    /// The one way to tell a cancelled stream from an unusable transport
    /// when [`send`](Self::send) returns
    /// [`StatusCode::InvalidOperation`].
    pub fn is_canceled(&self) -> bool {
        match &self.inner {
            SinkInner::Calls(p) => p.is_canceled(),
            SinkInner::Ring(p) => p.is_canceled(),
        }
    }

    /// Items accepted by [`send`](Self::send) or `send_async` and not
    /// yet on their way.
    ///
    /// On the RPC path, the pending batch: zero right after
    /// [`flush`](Self::flush) and after any `send` that crossed the byte
    /// threshold, and a non-zero reading while the consumer reports
    /// nothing arriving is the signature of a producer that should be
    /// flushing. On the ring, at most one: the record a dropped
    /// `send_async` future left waiting for room.
    pub fn pending(&self) -> usize {
        match &self.inner {
            SinkInner::Calls(p) => p.pending(),
            SinkInner::Ring(p) => p.pending(),
        }
    }
}

impl<T: ?Sized> std::fmt::Debug for Sink<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sink")
            .field(
                "transport",
                &match &self.inner {
                    SinkInner::Calls(_) => "rpc",
                    SinkInner::Ring(_) => "ring",
                },
            )
            .field("canceled", &self.is_canceled())
            .field("pending", &self.pending())
            .finish()
    }
}

// ---------------------------------------------------------------------
// Consumer
// ---------------------------------------------------------------------

enum ReceiverInner<T> {
    Calls(calls::Consumer<T>),
    Ring(ring::Consumer<T>),
}

/// The consumer's end of a stream: items come out in the order the
/// producer sent them.
///
/// Made by [`Receiver::new`] against the peer, which also produces the
/// endpoint to pass to the producer. That is the whole setup: from then
/// on the receiver reads, keeps the producer paced, and watches it for
/// death.
///
/// Consume with [`recv`](Self::recv), or by iterating: `Receiver`
/// implements [`Iterator`] over `BinderResult<T>`, ending when the
/// producer's terminator arrives.
pub struct Receiver<T> {
    inner: ReceiverInner<T>,
    /// The peer, kept for the unlink in `Drop`.
    peer: SIBinder,
    /// Holds the death link on the peer; the binder keeps only a `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    _item: PhantomData<fn() -> T>,
}

impl<T: Deserialize> Receiver<T> {
    /// A receiver and the endpoint to hand to the producer, with the
    /// default [`ReceiverPolicy`].
    ///
    /// `peer` is a binder in the producer's process — the service about
    /// to be called, or the binder an uploading client passed (see the
    /// [module docs](self#the-endpoint-and-the-call-that-opens-a-stream)).
    /// It picks the transport: a kernel proxy or a local object makes a
    /// ring endpoint, an RPC proxy a sink-only one. And it is watched for
    /// death, so a producer that dies releases a consumer blocked in
    /// [`recv`](Self::recv); a `peer` that is not in the producer's
    /// process leaves that death unseen, and a local object is not
    /// watched at all.
    ///
    /// Pass the endpoint to the method that starts the stream — declared
    /// in `.aidl` as `rsbinder.stream.StreamEndpoint`.
    ///
    /// # Errors
    ///
    /// [`StatusCode::BadValue`] for a ring no larger than
    /// [`END_RESERVE`]; whatever allocating the ring fails with (memfd,
    /// `fallocate`, seals); whatever linking to `peer`'s death fails with
    /// — [`StatusCode::DeadObject`] for a peer already gone.
    pub fn new(peer: &SIBinder) -> Result<(Self, StreamEndpoint)> {
        Self::with_policy(peer, &ReceiverPolicy::default())
    }

    /// [`new`](Self::new) with the policy set explicitly.
    pub fn with_policy(peer: &SIBinder, policy: &ReceiverPolicy) -> Result<(Self, StreamEndpoint)> {
        let (inner, ring, sink, death) = if over_ring(peer) {
            let (consumer, ring, sink) = ring::Consumer::new(policy)?;
            let death = watch_death(peer, consumer.death_recipient())?;
            (ReceiverInner::Ring(consumer), Some(ring), sink, death)
        } else {
            let (consumer, sink) = calls::Consumer::new(policy);
            let death = watch_death(peer, consumer.death_recipient())?;
            (ReceiverInner::Calls(consumer), None, sink, death)
        };
        let endpoint = StreamEndpoint {
            ring,
            sink: Some(sink),
        };
        let receiver = Receiver {
            inner,
            peer: peer.clone(),
            death,
            _item: PhantomData,
        };
        Ok((receiver, endpoint))
    }

    /// The endpoint again, for a caller that did not keep the one
    /// [`new`](Self::new) returned. A ring endpoint carries fresh
    /// duplicates of the ring's file descriptor.
    pub fn endpoint(&self) -> Result<StreamEndpoint> {
        let (ring, sink) = match &self.inner {
            ReceiverInner::Calls(c) => (None, c.sink_binder()),
            ReceiverInner::Ring(c) => (Some(c.ring()?), c.sink_binder()),
        };
        Ok(StreamEndpoint {
            ring,
            sink: Some(sink),
        })
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
    /// On the RPC path a grant that cannot be sent ends the stream, with
    /// the error the send failed with, only when nothing else could move
    /// it: this call is about to block, and the producer has used all
    /// the credit it is known to have. A producer with credit left sends
    /// again, and the grant is tried again when that batch is drained.
    pub fn recv(&mut self) -> Option<BinderResult<T>> {
        match &mut self.inner {
            ReceiverInner::Calls(c) => c.recv(),
            ReceiverInner::Ring(c) => c.recv(),
        }
    }

    /// The next item if one is already here, without waiting for one.
    ///
    /// `Ok(None)` means nothing has arrived yet and the stream is still
    /// running; the stream being over reads as `Ok(None)` too, which
    /// [`is_finished`](Self::is_finished) tells apart.
    ///
    /// On the ring this is one look at the ring. On the RPC path it never
    /// waits for an item, but it is not free: with credit owed and
    /// nothing left to drain it grants before answering, because a
    /// producer out of credit sends nothing that would prompt a later
    /// grant. That grant is one `oneway` call, and it waits for a free
    /// outgoing connection on the session — so an event loop polling this
    /// can be held up for as long as the session's outgoing connections
    /// are busy. A grant that fails here does not end the stream, as it
    /// may in [`recv`](Self::recv): this call returns, and the next one
    /// tries the grant again.
    pub fn try_recv(&mut self) -> BinderResult<Option<T>> {
        match &mut self.inner {
            ReceiverInner::Calls(c) => c.try_recv(),
            ReceiverInner::Ring(c) => c.try_recv(),
        }
    }

    /// [`recv`](Self::recv) with a bound on how long it waits.
    ///
    /// `Ok(None)` when `timeout` elapses with the stream still running.
    /// A timeout leaves the stream usable — call again.
    ///
    /// `timeout` bounds the wait for an item only. On the RPC path the
    /// grant [`try_recv`](Self::try_recv) describes is sent before that
    /// wait starts and is not counted against it, so this can return
    /// later than `timeout` by however long a grant takes; one that fails
    /// is tried again by the next call, not within this one.
    ///
    /// A `timeout` too long for an `Instant` to express — `Duration::MAX`
    /// — waits without a bound, and is [`recv`](Self::recv) in every
    /// respect.
    pub fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        match &mut self.inner {
            ReceiverInner::Calls(c) => c.recv_timeout(timeout),
            ReceiverInner::Ring(c) => c.recv_timeout(timeout),
        }
    }

    /// [`recv`](Self::recv) for an async consumer.
    ///
    /// Same result as `recv`, with the wait made from the blocking pool
    /// instead of on the executor thread: a futex wait on the ring, a
    /// credit grant on the RPC path. No `Stream` trait is implemented and
    /// none is in the public signature: that would bind rsbinder's API
    /// to a `futures-core` major version. Adapt it where you need one.
    ///
    /// Cancel-safe: a future dropped mid-wait loses no item. On the ring
    /// the wait it left on the pool is joined by the next call, so the
    /// wake it consumes is not lost; on the RPC path a grant already
    /// handed to the pool still goes out, and the next call may send the
    /// same running total again, which the producer counts once.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    pub async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        match &mut self.inner {
            ReceiverInner::Calls(c) => c.recv_async().await,
            ReceiverInner::Ring(c) => c.recv_async().await,
        }
    }

    /// Whether the stream is over — the terminator arrived and every
    /// item before it has been handed out.
    pub fn is_finished(&self) -> bool {
        match &self.inner {
            ReceiverInner::Calls(c) => c.is_finished(),
            ReceiverInner::Ring(c) => c.is_finished(),
        }
    }

    /// How the producer ended the stream, once it has.
    ///
    /// `None` while the stream is still running. After it ends this is
    /// the status the producer passed to [`Sink::end`], including
    /// `Status::ok()` for a stream that simply ran out.
    pub fn end_status(&self) -> Option<Status> {
        match &self.inner {
            ReceiverInner::Calls(c) => c.end_status(),
            ReceiverInner::Ring(c) => c.end_status(),
        }
    }

    /// Tell the producer to stop.
    ///
    /// Releases a producer parked waiting for room or for credit, and
    /// its [`Sink::send`] then reports [`StatusCode::InvalidOperation`].
    /// Items already written or in flight may still arrive, and the
    /// producer may still send a terminator. On the ring the producer
    /// sees it before its next write, or in the wait it is parked in; on
    /// the RPC path a cancel made before the producer has introduced
    /// itself is held and sent the moment it does.
    ///
    /// Sent automatically when a `Receiver` is dropped, so a consumer
    /// that walks away does not leave the producer waiting.
    pub fn cancel(&self) -> Result<()> {
        match &self.inner {
            ReceiverInner::Calls(c) => c.cancel(),
            ReceiverInner::Ring(c) => c.cancel(),
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
        // Before the inner consumer goes: dropping the recipient only
        // makes the link inert, and the peer's recipient list never
        // shrinks on its own.
        let death = self.death.take();
        unlink_death(&self.peer, &death);
    }
}

impl<T> std::fmt::Debug for Receiver<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut s = f.debug_struct("Receiver");
        match &self.inner {
            ReceiverInner::Calls(c) => s
                .field("transport", &"rpc")
                .field("finished", &c.is_finished())
                .field("started", &c.started()),
            ReceiverInner::Ring(c) => s
                .field("transport", &"ring")
                .field("finished", &c.is_finished()),
        };
        s.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;

    /// A pair through the public surface. A local peer is a kernel-class
    /// transport, so on Linux this is the ring; the RPC path's own
    /// end-to-end is `tests/stream_rpc.rs`.
    fn pair<T: Serialize + Deserialize>() -> (Sink<T>, Receiver<T>) {
        let peer = Token::new().binder();
        let (rx, endpoint) = Receiver::<T>::new(&peer).expect("a receiver");
        let sink = Sink::<T>::open(&endpoint).expect("open");
        (sink, rx)
    }

    #[test]
    fn a_local_peer_gets_a_ring_endpoint_on_linux() {
        let peer = Token::new().binder();
        let (rx, endpoint) = Receiver::<i32>::new(&peer).expect("a receiver");
        assert!(endpoint.sink.is_some());
        assert_eq!(
            endpoint.ring.is_some(),
            cfg!(any(target_os = "linux", target_os = "android"))
        );
        assert!(!rx.is_finished());
        // And again, from the receiver.
        let again = rx.endpoint().expect("endpoint");
        assert_eq!(again.ring.is_some(), endpoint.ring.is_some());
    }

    #[test]
    fn items_arrive_in_order_and_the_stream_ends_clean() {
        let (mut sink, mut rx) = pair::<i32>();
        let sent: Vec<i32> = (0..10).collect();
        sink.send_all(sent.iter()).expect("send_all");
        sink.end().expect("end");

        let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, sent);
        assert!(rx.is_finished());
        assert!(rx.end_status().expect("a terminator arrived").is_ok());
    }

    /// The failure a service would have returned, arriving after the
    /// method that started the stream already succeeded.
    #[test]
    fn a_service_specific_failure_survives_the_terminator() {
        let (mut sink, mut rx) = pair::<i32>();
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

    /// `EX_TRANSACTION_FAILED` says the binder layer failed, which a call
    /// that arrived cannot report, so the producer's own status is
    /// refused — and the stream still ends, on `EX_ILLEGAL_ARGUMENT`.
    #[test]
    fn a_terminator_that_cannot_be_carried_is_refused() {
        let (sink, mut rx) = pair::<i32>();
        assert_eq!(
            sink.end_with(&Status::from(ExceptionCode::TransactionFailed))
                .err(),
            Some(StatusCode::BadValue)
        );
        let ended = rx.next().expect("a terminator").expect_err("an error");
        assert_eq!(ended.exception_code(), ExceptionCode::IllegalArgument);
        assert_eq!(ended.message(), Some(UNCARRIABLE_TERMINATOR));
    }

    /// A producer that goes away mid-stream still ends the stream, because
    /// the consumer blocked in `recv` has no other way to find out.
    #[test]
    fn a_dropped_sink_ends_the_stream_as_failed() {
        let (mut sink, mut rx) = pair::<i32>();
        sink.send(&1).expect("send");
        sink.send(&2).expect("send");
        drop(sink);

        assert_eq!(rx.next().expect("item 1").expect("ok"), 1);
        assert_eq!(rx.next().expect("item 2").expect("ok"), 2);
        let failure = rx.next().expect("a terminator").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(rx.next().is_none(), "the stream is over");
    }

    #[test]
    fn a_cancel_releases_a_parked_producer_and_the_next_send_is_refused() {
        let peer = Token::new().binder();
        let (rx, endpoint) = Receiver::<i32>::with_policy(
            &peer,
            &ReceiverPolicy {
                ring_bytes: 512,
                credit_window: 1,
                max_opening: 1,
            },
        )
        .expect("a receiver");
        let mut sink = Sink::<i32>::open_with(
            &endpoint,
            &SinkPolicy {
                max_batch_bytes: 4,
                initial_credits: 1,
                ..SinkPolicy::default()
            },
        )
        .expect("open");

        let (outcome, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            let parked = loop {
                if let Err(e) = sink.send(&0) {
                    break e;
                }
            };
            let _ = outcome.send((parked, sink.is_canceled()));
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the producer must be parked"
        );
        rx.cancel().expect("cancel");
        let (parked, canceled) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel must release the producer");
        assert_eq!(parked, StatusCode::InvalidOperation);
        assert!(canceled);
        producer.join().expect("producer");
    }

    #[test]
    fn an_endpoint_without_a_sink_is_refused() {
        let endpoint = StreamEndpoint::default();
        assert_eq!(
            Sink::<i32>::open(&endpoint).err(),
            Some(StatusCode::BadValue)
        );
    }

    #[test]
    fn a_token_answers_no_transaction() {
        let token = Token::new();
        let binder = token.binder();
        assert!(binder.as_remote().is_none(), "a local object");
        assert_eq!(binder.descriptor(), "rsbinder.stream.Token");
        // The same object each time, so a service that compares the
        // argument with what it linked to sees one binder.
        assert_eq!(
            SIBinder::downgrade(&token.binder()),
            SIBinder::downgrade(&binder)
        );
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

    /// An item holding a binder cannot cross as bytes and is refused
    /// before anything is written.
    #[test]
    fn an_item_with_a_binder_is_refused() {
        let (mut sink, mut rx) = pair::<SIBinder>();
        let refused = sink
            .send(&Token::new().binder())
            .expect_err("no binder in an item");
        assert!(
            matches!(
                refused,
                StatusCode::BadType | StatusCode::FdsNotAllowed | StatusCode::InvalidOperation
            ),
            "{refused:?}"
        );
        sink.end().expect("end");
        assert!(rx.next().is_none(), "nothing was written");
    }
}

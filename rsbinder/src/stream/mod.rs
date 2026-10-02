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
//! # fn in_a_handler(endpoint: &StreamEndpoint<Row>) -> Result<()> {
//! // Service: called with the consumer's endpoint. The method itself
//! // has nothing to return — the stream carries everything. The
//! // endpoint's type argument makes this a `Sink<Row>`.
//! let mut sink = Sink::open(endpoint)?;
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
//! consumer does not hold an executor thread (see [Async](#async) for the
//! waits that hold a blocking-pool thread instead):
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
//! # fn subscribe(_service: &SIBinder, _endpoint: &StreamEndpoint<Row>) -> Result<()> { unimplemented!() }
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
//! `rsbinder.stream.StreamEndpoint<T>` (`rsbinder/aidl/stream/`), that a
//! service's own `.aidl` takes as an argument or returns. `rsbinder-aidl`
//! resolves an `import rsbinder.stream.StreamEndpoint;` to this type, so
//! no copy of the file is needed. One `twoway` call opens the stream, and
//! there is no second call:
//!
//! | The client is… | The call | Death links |
//! |---|---|---|
//! | the consumer (a download) | `void subscribe(in StreamEndpoint<Row> endpoint)` | `Receiver::new(&service)`; the producer links to `endpoint.sink` |
//! | the producer (an upload) | `StreamEndpoint<Row> upload(IBinder producer)` | `Receiver::new(&producer)` in the handler; the producer links to `endpoint.sink` |
//!
//! The type argument is the item type, and it is where the `.aidl` states
//! it: the generated method takes a `StreamEndpoint<Row>`, only a
//! `Receiver<Row>` makes one, and [`Sink::open`] on it is a `Sink<Row>`,
//! so both ends are held to the declaration at compile time. It is not on
//! the wire, so a C++ peer compiled from the same `.aidl` interoperates
//! whatever it names, and [`StreamEndpoint::cast`] lets a Rust end use a
//! type of its own that encodes the same way.
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
//! Which one a stream gets is the consumer's peer: a kernel proxy makes a
//! ring, and so does a local object on Linux and Android; an RPC proxy
//! makes a sink-only endpoint, and so does a local object elsewhere.
//! [`Sink::open`] follows whichever the endpoint carries.
//!
//! # What the two have in common
//!
//! * **Items carry no binder and no file descriptor.** An item is bytes
//!   in parcel encoding with no object table, so one holding either is
//!   refused at [`send`](Sink::send) — see [`crate::to_bytes`], which
//!   uses the same parcel mode.
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
//!   for an item yields it from [`Receiver::recv`]. On RPC a peer can
//!   vanish without the session noticing — behind a TCP relay — and a
//!   waiting end whose session has a reply deadline pings it to find out
//!   ([`PingPolicy`]).
//! * **Cancel.** [`Receiver::cancel`], and dropping a `Receiver`, release
//!   a parked producer, whose next [`Sink::send`] reports
//!   [`StatusCode::InvalidOperation`].
//! * **A consumer that neither reads nor cancels** is alive, so no death
//!   link fires, and by default a producer waits on it indefinitely. A
//!   service streaming to a client it does not trust sets
//!   [`SinkPolicy::send_timeout`], after which a wait for room or credit
//!   ends with [`StatusCode::TimedOut`] and the stream stays usable.
//!
//! # Async
//!
//! With the `tokio` feature, every `*_async` method makes its wait from
//! the blocking pool — a futex wait on the ring, a `oneway` send on RPC —
//! so a producer or consumer that has to wait does not hold an executor
//! thread. A futex cannot be polled, so on the ring each wait holds one
//! pool thread for as long as it lasts, the same cost as a thread of its
//! own. On RPC a wait that is bounded or may [ping](PingPolicy) holds a
//! pool thread the same way. **Poll them inside a Tokio runtime**:
//! outside one the hand-off panics, as `tokio::task::spawn_blocking` does. A call from inside a
//! transaction handler makes its wait on the calling thread instead and
//! is exempt.

use std::marker::PhantomData;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::binder::Interface;
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, Serialize};
use crate::status::{BinderResult, ExceptionCode, Status};
use crate::SIBinder;

// Private bar the endpoint: exposing it puts every generated trait under the stability promise.
mod generated {
    include!(concat!(env!("OUT_DIR"), "/stream.rs"));
}

mod calls;
#[cfg(feature = "tokio")]
mod pool;
mod ring;

pub use generated::rsbinder::stream::StreamEndpoint::StreamEndpoint;

impl<T> StreamEndpoint<T> {
    /// The same endpoint with `U` as its item type.
    ///
    /// The type argument is the `.aidl`'s statement of what the items are,
    /// and [`Receiver::new`] and [`Sink::open`] hold both ends to it. It
    /// is not on the wire — the endpoint is the same bytes whatever the
    /// item type — so a Rust type other than the generated one, encoding
    /// the same way, can stand in on either end: `cast` the endpoint the
    /// generated code handed over, or the one a receiver made before
    /// passing it on. A generated handler receives the endpoint by
    /// reference (`&StreamEndpoint<T>`); there, cast a copy:
    /// `endpoint.try_clone()?.cast::<U>()` (see [`try_clone`](Self::try_clone)).
    ///
    /// Nothing checks that the two encode alike. A mismatch the consumer's
    /// decoding notices ends the stream there with that error — among
    /// others [`StatusCode::NotEnoughData`] when an item's bytes run out,
    /// [`StatusCode::BadValue`] when some are left over,
    /// [`StatusCode::UnexpectedNull`] for a null marker where a value is
    /// required. One it does not notice — `i32` read as `f32`, two
    /// parcelables with the same fields — is read as the wrong value.
    ///
    /// Without `cast`, an endpoint opens a sink of its own item type only:
    ///
    /// ```compile_fail,E0308
    /// # use rsbinder::stream::{Receiver, Sink, Token};
    /// # fn f() -> rsbinder::Result<()> {
    /// let (_rx, endpoint) = Receiver::<i32>::new(&Token::new().binder())?;
    /// let _sink = Sink::<i64>::open(&endpoint)?; // expected `StreamEndpoint<i64>`
    /// # Ok(())
    /// # }
    /// ```
    pub fn cast<U>(self) -> StreamEndpoint<U> {
        StreamEndpoint {
            ring: self.ring,
            sink: self.sink,
            _phantom_T: PhantomData,
        }
    }

    /// A second endpoint for the same stream: the same sink binder and, on
    /// the kernel path, fresh duplicates of the ring's file descriptors.
    ///
    /// The generated server trait hands a handler `&StreamEndpoint<T>`,
    /// which [`Sink::open`] takes as it is; `try_clone` is for a handler
    /// that needs an endpoint of its own — to [`cast`](Self::cast) it to
    /// another item type, or to keep it past the call.
    ///
    /// # Errors
    ///
    /// [`StatusCode::BadValue`] for a ring descriptor that does not
    /// describe a synchronized ring, which [`Sink::open`] refuses too;
    /// whatever duplicating a file descriptor fails with.
    pub fn try_clone(&self) -> Result<Self> {
        let ring = match &self.ring {
            Some(ring) => Some(crate::fmq::Descriptor::try_from(ring)?.try_into()?),
            None => None,
        };
        Ok(StreamEndpoint {
            ring,
            sink: self.sink.clone(),
            _phantom_T: PhantomData,
        })
    }

    fn with(
        ring: Option<crate::fmq::MQDescriptor<i8, crate::fmq::SynchronizedReadWrite>>,
        sink: SIBinder,
    ) -> Self {
        StreamEndpoint {
            ring,
            sink: Some(sink),
            _phantom_T: PhantomData,
        }
    }
}

/// Bytes at the end of a ring that item records never occupy, so that the
/// end record — at most this long, its message cut to fit — always has
/// room and [`Sink::end`] never waits for the consumer to make it.
///
/// 256 holds the 4-byte header, the two `int` status fields and a line of
/// message, and is 0.4% of the default ring. A ring must exceed this by
/// more than an item header (`END_RESERVE + 4`), and its largest item is
/// `ring_bytes - END_RESERVE - 4`.
pub const END_RESERVE: usize = 256;

/// What a [`Receiver`] is made with: the ring on the kernel path, the
/// credit window on the RPC path. The peer decides which applies.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReceiverPolicy {
    /// Kernel binder: bytes of ring the consumer allocates, and so the
    /// most the producer can run ahead by. Must exceed [`END_RESERVE`]` + 4`;
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
    /// RPC: whether a wait for an item that hears nothing from the
    /// producer checks that it is still there. Default
    /// [`PingPolicy::Inherit`].
    pub ping: PingPolicy,
}

impl Default for ReceiverPolicy {
    fn default() -> Self {
        ReceiverPolicy {
            ring_bytes: 64 * 1024,
            credit_window: 4,
            max_opening: 4,
            ping: PingPolicy::default(),
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
    /// Both paths: how long one call may wait for the consumer — for room
    /// in the ring, or for credit on the RPC path.
    ///
    /// `None`, the default, waits for as long as it takes. A consumer that
    /// is alive but neither reads nor cancels then holds the producer's
    /// thread for good — or, for an `*_async` call on the ring or one on
    /// RPC that may [ping](PingPolicy), a blocking-pool thread — which a
    /// service streaming to an untrusted
    /// client should not allow.
    ///
    /// `Some(d)` sets one deadline per call, `d` after a blocking method
    /// is called or after an `*_async` future is first polled, and every
    /// wait for room or credit that call makes shares it:
    /// [`send_all`](Sink::send_all) has one deadline for all its items,
    /// not one per item. On expiry the call returns
    /// [`StatusCode::TimedOut`], and no thread is left parked past the
    /// deadline, the pool thread of an `*_async` call included. It bounds
    /// only waits for room or credit: the RPC transaction that carries a
    /// batch or the terminator waits under the session's own deadline
    /// (`RpcSession::set_timeout`, or `RpcServer::set_reply_timeout` on a
    /// server), and a batch a dropped `*_async` future left sending is
    /// waited for under that deadline too.
    ///
    /// What a timeout leaves behind:
    ///
    /// * [`send`](Sink::send): the item was not written (ring) or not
    ///   queued (RPC), and items accepted before it are unaffected. The
    ///   stream stays usable: send it again, or [`end`](Sink::end).
    /// * [`flush`](Sink::flush), on the RPC path: the pending batch stays
    ///   pending, whole, and a later successful flush — or `end` — sends
    ///   it. On the ring, flush waits only for a record a dropped
    ///   `send_async` future left on the pool; that record waits under the
    ///   deadline of the call that made it, and one whose deadline expires
    ///   is counted lost, which `end` reports.
    /// * [`end`](Sink::end) and [`end_with`](Sink::end_with): the end
    ///   record never waits for room ([`END_RESERVE`]) and the RPC
    ///   terminator needs no credit, so what times out is only what had to
    ///   go before it — the pending batch on RPC, a dropped future's
    ///   record on the ring. That is given up and counted lost, the stream
    ///   still ends, as `EX_ILLEGAL_STATE` saying how many items were not
    ///   sent, and the call returns `TimedOut`.
    ///
    /// `Some(Duration::ZERO)` never waits: a call writes or sends if room
    /// or credit is there now and returns `TimedOut` otherwise. A duration
    /// too long for an `Instant` to express waits without a bound, as
    /// `None` does. On the RPC path a bounded `*_async` credit wait holds
    /// a blocking-pool thread while it lasts (this build has no timer to
    /// suspend the task against); an unbounded one suspends the task,
    /// unless it may [ping](PingPolicy), which holds a pool thread too.
    ///
    /// Nor does it count a [ping](PingPolicy) made while waiting for
    /// credit: a call whose consumer stops answering can return later than
    /// `d`, by up to the session's reply deadline. A `d` shorter than a
    /// third of the reply deadline ends each wait before any ping, so a
    /// producer that only makes such calls never checks the consumer.
    pub send_timeout: Option<Duration>,
    /// RPC: whether a wait for credit that hears nothing from the consumer
    /// checks that it is still there. Default [`PingPolicy::Inherit`].
    pub ping: PingPolicy,
}

impl Default for SinkPolicy {
    fn default() -> Self {
        SinkPolicy {
            max_ring_bytes: 4 * 1024 * 1024,
            max_batch_bytes: 16 * 1024,
            initial_credits: 4,
            send_timeout: None,
            ping: PingPolicy::default(),
        }
    }
}

/// Whether a stream on the RPC path checks that a peer it is waiting on
/// is still there (plan 2-24 D9).
///
/// A stream learns that its peer is gone from the peer's death, and over
/// RPC a death is the session ending, which takes the transport noticing
/// that the connection is gone. Behind a relay that terminates TCP —
/// `adb forward`, `ssh -L`, a port-forwarding proxy — the transport may
/// never notice, because the relay keeps acknowledging; kernel keepalive
/// reaches only the relay. Nor is there a call whose reply deadline could
/// expire: back-pressure means a waiting stream has nothing in flight,
/// and every `IStreamSink` and `IStreamSource` method is `oneway`. Such a
/// stream would wait for good.
///
/// With pinging on, a wait that nothing has woken for a third of the
/// session's reply deadline (`RpcSession::set_timeout`, or
/// `RpcServer::set_reply_timeout` on a server) sends the peer a
/// `PING_TRANSACTION` — the twoway every binder object answers,
/// rsbinder's and libbinder's alike, so the peer implements nothing — and
/// goes back to waiting once it is answered. The consumer pings the
/// producer's `IStreamSource`, the producer the consumer's sink. A peer
/// that does not answer within the reply deadline ends the session, as
/// any unanswered call does, and the stream then ends with
/// [`StatusCode::DeadObject`] through its death link: a peer that has
/// vanished is noticed within about four thirds of the deadline. The ping
/// is an ordinary call, so it also waits for a free outgoing connection;
/// one refused for want of one is dropped, and the next quiet third tries
/// again.
///
/// Only a wait pings: [`Receiver::recv`] and its variants waiting for an
/// item, once the producer has introduced itself, and a [`Sink`] call
/// waiting for credit. A stream whose items flow never pings, and an end
/// that is not waiting on the stream checks nothing until it next waits.
/// A wait whose own bound is shorter than a third of the reply deadline
/// ends without pinging, so a loop of such calls never checks the peer.
/// The ping is not counted against the caller's own bound —
/// [`Receiver::recv_timeout`]'s `timeout`, [`SinkPolicy::send_timeout`] —
/// so a call whose peer stops answering can return later than that bound,
/// by up to the reply deadline. With the `tokio` feature, an `*_async`
/// wait that may ping is made on the blocking pool and holds a thread
/// there for as long as it lasts; this build has no timer to suspend a
/// task against.
///
/// The ring path never pings: the kernel reports the peer's death itself.
#[non_exhaustive]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum PingPolicy {
    /// Ping when the peer's session has a reply deadline, after a third of
    /// it passes quietly. A session without one — the default — is never
    /// pinged, and neither is a peer that is not an RPC proxy.
    #[default]
    Inherit,
    /// Never ping.
    Off,
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

// --- Shared by both paths ---

/// Link `recipient` to `binder`'s death, held alive by the returned `Arc`; `Ok(None)` if local.
fn watch_death<R>(binder: &SIBinder, recipient: R) -> Result<Option<Arc<dyn crate::DeathRecipient>>>
where
    R: crate::DeathRecipient + 'static,
{
    if binder.as_remote().is_none() {
        return Ok(None);
    }
    let recipient: Arc<dyn crate::DeathRecipient> = Arc::new(recipient);
    // Fatal: back-pressure leaves nothing in flight to fail, so only this link reports death.
    if let Err(e) = binder.link_to_death(Arc::downgrade(&recipient)) {
        if e == StatusCode::InvalidOperation && binder.as_remote().is_some() {
            log::error!(
                "stream: the RPC session to the other end has no incoming connections, so the \
                 stream could neither be pushed to nor see that end die; open them \
                 (ClientOptions::incoming_connections / RpcClientConfig::incoming_connections)"
            );
        } else {
            log::error!("stream: cannot watch the peer for death: {e:?}");
        }
        return Err(e);
    }
    Ok(Some(recipient))
}

/// Undo `watch_death` (no lock held): a dropped `Arc` alone leaves the kernel subscription.
fn unlink_death(binder: &SIBinder, recipient: &Option<Arc<dyn crate::DeathRecipient>>) {
    let Some(recipient) = recipient else { return };
    // `Drop` calls this; a panic here during an unwind (poisoned recipients lock) would abort.
    let unlinked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = binder.unlink_to_death(Arc::downgrade(recipient));
    }));
    if unlinked.is_err() {
        log::error!("stream: unlink_to_death panicked; a death link stays registered");
    }
}

/// An RPC proxy answers from its session; anything else is local or kernel and always callable.
fn peer_caps(binder: &SIBinder) -> crate::TransportCaps {
    #[cfg(feature = "rpc")]
    if let Some(proxy) = (**binder).as_any().downcast_ref::<crate::rpc::RpcProxy>() {
        return proxy.session_caps();
    }
    #[cfg(not(feature = "rpc"))]
    let _ = binder;
    crate::TransportCaps::KERNEL
}

/// A ring needs one kernel driver (or process) for both ends plus OS shared memory and futex.
fn over_ring(peer: &SIBinder) -> bool {
    cfg!(any(target_os = "linux", target_os = "android"))
        && peer_caps(peer).contains(crate::TransportCaps::KERNEL_KNOBS)
}

/// The deadline one `Sink` call's waits share; `None` = no bound, also past what `Instant` holds.
fn call_deadline(timeout: Option<Duration>) -> Option<Instant> {
    timeout.and_then(|timeout| Instant::now().checked_add(timeout))
}

/// One item as record bytes in `scratch`, emptied first; a data-only parcel refuses a binder or an fd.
fn encode_item<T: Serialize + ?Sized>(scratch: &mut Parcel, item: &T) -> Result<()> {
    scratch.set_data_size(0)?;
    scratch.write(item)
}

/// One item from all of `buf`, read in place; `buf` keeps its allocation for the next record.
fn decode_item<T: Deserialize>(buf: &mut Vec<u8>) -> Result<T> {
    let mut parcel = Parcel::data_only_from_vec(std::mem::take(buf));
    let item = parcel.read::<T>();
    let left = parcel.data_avail();
    // Data-only, so self-contained: the Vec moves back.
    if let Ok(bytes) = parcel.into_bytes() {
        *buf = bytes;
    }
    let item = item?;
    if left != 0 {
        log::error!(
            "stream: {left} bytes left after one item; the two ends disagree on the item type"
        );
        return Err(StatusCode::BadValue);
    }
    Ok(item)
}

/// Ends the stream when [`Sink::end_with`] got a status the wire cannot carry.
const UNCARRIABLE_TERMINATOR: &str =
    "the stream's producer ended with a status that cannot be carried";

/// Terminator for lost items; a producer's own failure, being more specific, takes precedence.
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

/// Inverse of [`status_fields`]; like AOSP, reads `service_specific` only for EX_SERVICE_SPECIFIC.
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
        // -127/-128 are reply-header markers, -129 a binder-layer failure: none arrives in a call.
        other => {
            log::error!("stream: {other} is not a valid stream terminator exception code");
            return Err(StatusCode::BadValue);
        }
    })
}

// --- Producer ---

enum SinkInner<T: ?Sized> {
    /// Boxed: the pending batch and credit state make it several times the ring producer's size.
    Calls(Box<calls::Producer<T>>),
    Ring(ring::Producer<T>),
}

/// The producer's handle on a stream: write items in, they reach the
/// consumer in order.
///
/// Made by [`Sink::open`] from the endpoint the consumer supplied, whose
/// type argument is `T`: a `StreamEndpoint<LogLine>` opens a
/// `Sink<LogLine>`. Values of `T` are encoded with the same codec as
/// [`crate::to_bytes`], so the consumer's item type need only agree on
/// the wire, not be the same Rust type — [`StreamEndpoint::cast`] says so
/// where the two differ. [`open_borrowed`](Self::open_borrowed) opens a
/// sink of the item type's borrowed form, a `Sink<str>` from a
/// `StreamEndpoint<String>`.
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
    /// [`SinkPolicy::send_timeout`]; each call turns it into a deadline.
    send_timeout: Option<Duration>,
}

impl<T: Serialize> Sink<T> {
    /// Take the consumer's endpoint and start a stream, with the default
    /// [`SinkPolicy`].
    ///
    /// # Errors
    ///
    /// - [`StatusCode::BadValue`] for an endpoint with no sink, a ring the
    ///   producer will not map (larger than
    ///   [`SinkPolicy::max_ring_bytes`], not sealed against shrinking,
    ///   no EventFlag word, or a descriptor that does not describe a
    ///   ring), or a ring no larger than [`END_RESERVE`]` + 4`.
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
    pub fn open(endpoint: &StreamEndpoint<T>) -> Result<Self> {
        Self::open_with(endpoint, &SinkPolicy::default())
    }

    /// [`open`](Self::open) with the policy set explicitly.
    ///
    /// # Errors
    ///
    /// Everything [`open`](Self::open) returns, plus, on the RPC path,
    /// [`StatusCode::BadValue`] for an [`initial_credits`](SinkPolicy::initial_credits)
    /// of zero, or too large for the `int` that carries it. A window the
    /// consumer refuses is not an error here — `onStart` is `oneway` —
    /// but a cancel, which [`send`](Self::send) reports when it next has
    /// a batch to send; the reason is on the consumer's side.
    pub fn open_with(endpoint: &StreamEndpoint<T>, policy: &SinkPolicy) -> Result<Self> {
        Self::open_borrowed(endpoint, policy)
    }
}

impl<T: Serialize + ?Sized> Sink<T> {
    /// [`open_with`](Self::open_with) for a sink that sends a borrowed form
    /// of the endpoint's item type: a `Sink<str>` from a
    /// `StreamEndpoint<String>`, so that a producer holding `&str` lines
    /// sends them without allocating a `String` for each.
    ///
    /// `E: Borrow<T>` is the whole check, so the borrowed form must encode
    /// as the owned one does. It does for the pairs the standard library
    /// provides that an item type can have — `String`/`str`, `Vec<U>`/`[U]`,
    /// `Box<U>`/`U` — and a type of the caller's own that implements
    /// `Borrow` with a different encoding fails at the consumer, as
    /// [`StreamEndpoint::cast`] describes.
    ///
    /// ```no_run
    /// # use rsbinder::stream::{Sink, SinkPolicy, StreamEndpoint};
    /// # fn handler(endpoint: &StreamEndpoint<String>, text: &str) -> rsbinder::Result<()> {
    /// let mut sink = Sink::<str>::open_borrowed(endpoint, &SinkPolicy::default())?;
    /// for line in text.lines() {
    ///     sink.send(line)?;
    /// }
    /// sink.end()
    /// # }
    /// ```
    ///
    /// # Errors
    ///
    /// Everything [`open_with`](Self::open_with) returns.
    pub fn open_borrowed<E: std::borrow::Borrow<T>>(
        endpoint: &StreamEndpoint<E>,
        policy: &SinkPolicy,
    ) -> Result<Self> {
        let Some(sink) = endpoint.sink.as_ref() else {
            log::error!("Sink::open: the endpoint has no sink");
            return Err(StatusCode::BadValue);
        };
        let inner = match &endpoint.ring {
            Some(ring) => SinkInner::Ring(ring::Producer::open(ring, sink, policy)?),
            None => SinkInner::Calls(Box::new(calls::Producer::open(sink, policy)?)),
        };
        Ok(Sink {
            inner,
            send_timeout: policy.send_timeout,
        })
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
    /// go and the consumer has granted no credit for it. Either wait is
    /// bounded by [`SinkPolicy::send_timeout`] when that is set.
    ///
    /// # Errors
    ///
    /// On the ring an error always means this item was not written,
    /// whichever write failed — this call's, or a record a dropped
    /// [`send_async`](Self::send_async) left on the pool, whose failure
    /// the next call reports before writing anything.
    ///
    /// - [`StatusCode::TimedOut`] when [`SinkPolicy::send_timeout`]
    ///   expired before there was room (ring) or credit for the batch
    ///   this item completed (RPC): the item was not written or queued,
    ///   and the stream stays usable. On the RPC path a batch send the
    ///   session timed out returns it too (the last entry); that batch did
    ///   not arrive, its items are counted lost, and the session has ended.
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
    /// - Anything else is a batch send on the RPC path failing: the one
    ///   this call made when the pending batch reached its threshold, or
    ///   one a dropped `send_async` or [`flush_async`](Self::flush_async)
    ///   left on the pool, whose failure the next call reports. The failed
    ///   batch's items are lost, and [`end`](Self::end) tells the consumer
    ///   how many, so the stream ends as failed either way. What follows
    ///   depends on whether the batch arrived:
    ///   - It did not, and the session is up — a refused transaction
    ///     ([`StatusCode::FailedTransaction`]), or a call refused before
    ///     anything was sent for want of a free connection
    ///     ([`StatusCode::WouldBlock`]). Its credit comes back and the
    ///     stream stays usable. When the failed batch was an earlier one,
    ///     this call's item was queued after it and is still pending; it
    ///     goes out with the next flush. [`pending`](Self::pending) tells
    ///     the two cases apart.
    ///   - It did not, and the session has ended — a dead session
    ///     ([`StatusCode::DeadObject`]), or a send the session's deadline
    ///     or the kernel's keepalive check stopped
    ///     ([`StatusCode::TimedOut`]). Its credit comes back, but the
    ///     session's end marks the consumer dead, so later calls return
    ///     [`StatusCode::DeadObject`].
    ///   - Unknown — any other failure. How much credit is left is then
    ///     unknown too, and the consumer ends a stream on a batch sent
    ///     without credit, so nothing more is sent: this and every later
    ///     [`send`](Self::send) and [`flush`](Self::flush) return the same
    ///     error without queueing anything, and items still pending are
    ///     counted as lost at [`end`](Self::end).
    pub fn send(&mut self, item: &T) -> Result<()> {
        let deadline = call_deadline(self.send_timeout);
        self.send_until(item, deadline)
    }

    fn send_until(&mut self, item: &T, deadline: Option<Instant>) -> Result<()> {
        match &mut self.inner {
            SinkInner::Calls(p) => p.send(item, deadline),
            SinkInner::Ring(p) => p.send(item, deadline),
        }
    }

    /// [`send`](Self::send) for each item in turn, stopping at the first
    /// failure.
    ///
    /// One call, so one [`send_timeout`](SinkPolicy::send_timeout)
    /// deadline for all the items. On `TimedOut` the items before the one
    /// that timed out were accepted, and that one and the rest were not.
    pub fn send_all<'a, I>(&mut self, items: I) -> Result<()>
    where
        I: IntoIterator<Item = &'a T>,
        T: 'a,
    {
        let deadline = call_deadline(self.send_timeout);
        for item in items {
            self.send_until(item, deadline)?;
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
    /// something to send; it sends nothing, and cannot block, when nothing
    /// is queued. There, batches leave on the byte threshold and on this
    /// call, never on a clock: a producer whose items arrive at their own
    /// pace — an event feed, a log tail — calls this after each event or
    /// each burst, or an item sits in the pending batch until the next
    /// [`send`](Self::send) fills it, which for an idle producer is not a
    /// bounded wait. Flushing after every item costs a producer on the
    /// ring nothing, so a producer unsure which path it is on can simply
    /// flush.
    ///
    /// The errors are those of [`send`](Self::send), including a
    /// failure a dropped future left behind: this call reports it before
    /// sending anything, and what it leaves pending follows the same
    /// rules. On the RPC path with nothing queued, such a failure is
    /// reported only once the dropped future's batch has settled; this
    /// call does not wait for it.
    ///
    /// With [`SinkPolicy::send_timeout`] set, a flush that runs out of
    /// time returns [`StatusCode::TimedOut`]. On the RPC path, where the
    /// wait is for credit, the pending batch stays pending, whole: nothing
    /// is lost, and the next flush tries again. On the ring the wait is
    /// for a dropped future's record, which that field's entry covers.
    pub fn flush(&mut self) -> Result<()> {
        let deadline = call_deadline(self.send_timeout);
        match &mut self.inner {
            SinkInner::Calls(p) => p.flush(deadline),
            SinkInner::Ring(p) => p.flush(deadline),
        }
    }

    /// Finish the stream: the items ran out, nothing went wrong.
    ///
    /// On the RPC path queued items are flushed first, and that flush
    /// blocks for credit — unless the consumer has cancelled, in which
    /// case they are dropped and only the terminator goes out. On the
    /// ring the end record has room reserved for it ([`END_RESERVE`]),
    /// so this never waits for the consumer — unless a `send_async`
    /// future was dropped with its record still waiting for room, in
    /// which case it waits for that record first.
    ///
    /// Call it on every path, including the failing ones —
    /// [`end_with`](Self::end_with) is that path. A `Sink` dropped
    /// without either ends the stream as having failed, which is the
    /// only honest reading of a producer that vanished.
    ///
    /// With [`SinkPolicy::send_timeout`] set, those waits share one
    /// deadline. If it expires, what was waiting — the pending batch, or
    /// the dropped future's record — is given up and counted lost, the
    /// stream still ends, as `EX_ILLEGAL_STATE` saying how many items
    /// were not sent, and this returns [`StatusCode::TimedOut`].
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
        let deadline = call_deadline(self.send_timeout);
        match self.inner {
            SinkInner::Calls(p) => p.terminate(deadline, exception, service_specific, message),
            SinkInner::Ring(p) => p.terminate(deadline, exception, service_specific, message),
        }
    }

    /// [`send`](Self::send) for a producer running as a task.
    ///
    /// Same contract, with the wait moved off the executor thread: on the
    /// ring a record that finds no room is written from the blocking
    /// pool; on the RPC path the credit wait follows the
    /// [module docs](self#async) — a wait that is bounded or may ping
    /// holds a pool thread, any other suspends the task — and the batch is
    /// sent from the pool, the way every generated async proxy sends
    /// ([`Tokio`](crate::Tokio)).
    ///
    /// The item is encoded **when this is called**, not when the future
    /// is first polled, so the future does not borrow `item` and stays
    /// `Send` whatever `T` is. Dropping the future unpolled therefore
    /// leaves the item queued (RPC) or unwritten (ring). Dropping it
    /// mid-wait is safe too: a record or batch already handed to the pool
    /// still goes out, in its place, and the next call waits for it
    /// first. On the ring, a future dropped while it still waits for such
    /// an earlier record drops its own item, and the terminator counts it
    /// as lost: [`pending`](Self::pending) cannot tell the two cases apart,
    /// so the stream does not end clean with the item missing.
    ///
    /// [`SinkPolicy::send_timeout`] counts from the first poll. A record
    /// handed to the pool waits there under that deadline and no longer,
    /// whether or not the future is still awaited: an awaited one that
    /// expires returns [`StatusCode::TimedOut`] with the item unwritten,
    /// as [`send`](Self::send) does; a dropped one's record is counted
    /// lost.
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
        let timeout = self.send_timeout;
        match &mut self.inner {
            SinkInner::Calls(p) => pool::Either::A(p.send_async(item, timeout)),
            SinkInner::Ring(p) => pool::Either::B(p.send_async(item, timeout)),
        }
    }

    /// [`flush`](Self::flush) for a producer running as a task.
    ///
    /// [`SinkPolicy::send_timeout`] counts from the first poll.
    ///
    /// # Panics
    ///
    /// Outside a Tokio runtime — see the [module docs](self#async).
    #[cfg(feature = "tokio")]
    pub async fn flush_async(&mut self) -> Result<()> {
        let deadline = call_deadline(self.send_timeout);
        match &mut self.inner {
            SinkInner::Calls(p) => p.flush_async(deadline).await,
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
    /// [`SinkPolicy::send_timeout`] counts from the first poll, with the
    /// outcome [`end`](Self::end) describes.
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
        let deadline = call_deadline(self.send_timeout);
        match self.inner {
            SinkInner::Calls(p) => {
                p.terminate_async(deadline, exception, service_specific, message)
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
    /// On the RPC path, the pending batch: zero right after a
    /// [`flush`](Self::flush), or a `send` that crossed the byte
    /// threshold, that returned `Ok`, and a non-zero reading while the consumer reports
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

// --- Consumer ---

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
    /// It picks the transport: a kernel proxy makes a ring endpoint, and so
    /// does a local object on Linux and Android; an RPC proxy makes a
    /// sink-only one, and so does a local object elsewhere. And it is
    /// watched for death, so a producer that dies releases a consumer
    /// blocked in [`recv`](Self::recv); a `peer` that is not in the
    /// producer's process leaves that death unseen, and a local object is
    /// not watched at all.
    ///
    /// Pass the endpoint to the method that starts the stream — declared
    /// in `.aidl` as `rsbinder.stream.StreamEndpoint<T>`, so the method's
    /// signature fixes the item type of the receiver that can call it.
    ///
    /// # Errors
    ///
    /// [`StatusCode::BadValue`] for a ring no larger than
    /// [`END_RESERVE`]` + 4`; whatever allocating the ring fails with (memfd,
    /// `fallocate`, seals); whatever linking to `peer`'s death fails with
    /// — [`StatusCode::DeadObject`] for a peer already gone, and
    /// [`StatusCode::InvalidOperation`] for an RPC `peer` whose client
    /// session has no incoming connections
    /// ([`ClientOptions::incoming_connections`](crate::ClientOptions::incoming_connections)),
    /// which a stream over RPC needs in either direction: nothing would
    /// read the producer's batches, nor notice that end dying.
    pub fn new(peer: &SIBinder) -> Result<(Self, StreamEndpoint<T>)> {
        Self::with_policy(peer, &ReceiverPolicy::default())
    }

    /// [`new`](Self::new) with the policy set explicitly.
    pub fn with_policy(
        peer: &SIBinder,
        policy: &ReceiverPolicy,
    ) -> Result<(Self, StreamEndpoint<T>)> {
        let (inner, ring, sink, death) = if over_ring(peer) {
            let (consumer, ring, sink) = ring::Consumer::new(policy)?;
            let death = watch_death(peer, consumer.death_recipient())?;
            (ReceiverInner::Ring(consumer), Some(ring), sink, death)
        } else {
            let (consumer, sink) = calls::Consumer::new(policy);
            let death = watch_death(peer, consumer.death_recipient())?;
            (ReceiverInner::Calls(consumer), None, sink, death)
        };
        let endpoint = StreamEndpoint::with(ring, sink);
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
    pub fn endpoint(&self) -> Result<StreamEndpoint<T>> {
        let (ring, sink) = match &self.inner {
            ReceiverInner::Calls(c) => (None, c.sink_binder()),
            ReceiverInner::Ring(c) => (Some(c.ring()?), c.sink_binder()),
        };
        Ok(StreamEndpoint::with(ring, sink))
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
    /// `timeout` starts when the call does. On the RPC path the grant
    /// [`try_recv`](Self::try_recv) describes is sent within `timeout`:
    /// the time it takes shortens the wait for an item, and a grant that
    /// outlasts `timeout` makes this return late by the difference. One
    /// that fails is sent again at most once within this call, the next
    /// time the wait wakes — a ping's return included — with nothing
    /// queued, and that retry can make this return late the same way;
    /// past it, the next call tries the grant again. A
    /// [ping](PingPolicy) made while waiting is not counted against
    /// `timeout`, and a
    /// `timeout` shorter than a third of the reply deadline ends the wait
    /// before any ping, so a loop of such calls never checks the producer.
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
    /// Same result as `recv`, without blocking the executor thread. The
    /// blocking pool runs a futex wait on the ring; on the RPC path it
    /// runs a credit grant and a wait for an item that may
    /// [ping](PingPolicy), and any other wait suspends the task
    /// ([module docs](self#async)). No `Stream` trait is implemented and
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
    /// `None` until the end has been read: a death notice or terminator
    /// that lands while items remain is not yet the end, and those items
    /// come out of [`recv`](Self::recv) first. After it ends this is
    /// the status the stream ended on: the one passed to [`Sink::end`] or
    /// [`Sink::end_with`] (`Status::ok()` for a stream that ran out),
    /// except: `EX_ILLEGAL_STATE` when the producer lost queued items or
    /// dropped its [`Sink`] without ending (the message says how many
    /// items were not sent, possibly zero), `EX_ILLEGAL_ARGUMENT` when
    /// [`Sink::end_with`] was given a status the wire cannot carry,
    /// [`StatusCode::DeadObject`] when the producer died before its end
    /// reached this side (an end record already in the ring, or an
    /// `onEnd` already received, is the end instead), and the
    /// transport's own error when this side refused what it read or could
    /// not wait or grant.
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
        // Unlink first: dropping the recipient only makes the link inert, the peer's list stays.
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

    /// A local peer is kernel-class, so this is the ring on Linux; RPC is `tests/stream_rpc.rs`.
    fn pair<T: Serialize + Deserialize>() -> (Sink<T>, Receiver<T>) {
        let peer = Token::new().binder();
        let (rx, endpoint) = Receiver::<T>::new(&peer).expect("a receiver");
        let sink = Sink::<T>::open(&endpoint).expect("open");
        (sink, rx)
    }

    #[test]
    fn a_reused_buffer_is_neither_lost_nor_left_with_the_last_item() {
        let mut scratch = Parcel::new_data_only();
        encode_item(&mut scratch, &vec![9u8; 300]).expect("encode");
        encode_item(&mut scratch, &7i32).expect("encode over it");
        let mut buf = scratch.as_bytes().expect("bytes").to_vec();
        assert_eq!(buf.len(), 4, "nothing of the earlier item is left");
        let at = buf.as_ptr();
        assert_eq!(decode_item::<i32>(&mut buf), Ok(7));
        assert_eq!(buf.as_ptr(), at, "decode_item hands the buffer back");
        // A short read is refused, and the buffer still comes back.
        assert_eq!(
            decode_item::<i64>(&mut buf).err(),
            Some(StatusCode::NotEnoughData)
        );
        assert_eq!(buf.as_ptr(), at, "handed back after a short read");
        buf.extend_from_slice(&[0; 4]);
        let at = buf.as_ptr();
        assert_eq!(
            decode_item::<i32>(&mut buf).err(),
            Some(StatusCode::BadValue)
        );
        assert_eq!(buf.as_ptr(), at, "handed back after leftover bytes");
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

    /// A consumer's own type, encoded as the declared `i32`, stands in through `cast`.
    #[test]
    fn a_cast_endpoint_carries_a_type_that_encodes_alike() {
        #[derive(Debug, PartialEq)]
        struct Celsius(i32);
        impl Deserialize for Celsius {
            fn deserialize(parcel: &mut Parcel) -> Result<Self> {
                parcel.read::<i32>().map(Celsius)
            }
        }
        let peer = Token::new().binder();
        let (mut rx, endpoint) = Receiver::<Celsius>::new(&peer).expect("a receiver");
        let endpoint: StreamEndpoint<i32> = endpoint.cast();
        let mut sink = Sink::open(&endpoint).expect("open");
        sink.send_all([20, 21].iter()).expect("send_all");
        sink.end().expect("end");

        let got: Vec<Celsius> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, [Celsius(20), Celsius(21)]);
    }

    /// A handler holds only `&StreamEndpoint<T>`; its own type goes through a cast copy.
    #[test]
    fn a_borrowed_endpoint_casts_through_try_clone() {
        struct Celsius(i32);
        impl Serialize for Celsius {
            fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
                parcel.write(&self.0)
            }
        }
        fn handler(endpoint: &StreamEndpoint<i32>) -> Result<Sink<Celsius>> {
            Sink::open(&endpoint.try_clone()?.cast())
        }
        let peer = Token::new().binder();
        let (mut rx, endpoint) = Receiver::<i32>::new(&peer).expect("a receiver");
        let mut sink = handler(&endpoint).expect("open");
        drop(endpoint);
        sink.send_all([Celsius(20), Celsius(21)].iter())
            .expect("send_all");
        sink.end().expect("end");

        let got: Vec<i32> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, [20, 21]);
    }

    /// A `StreamEndpoint<String>` producer sends `&str` without an owned `String` per item.
    #[test]
    fn a_borrowed_item_type_opens_through_open_borrowed() {
        let peer = Token::new().binder();
        let (mut rx, endpoint) = Receiver::<String>::new(&peer).expect("a receiver");
        let mut sink = Sink::<str>::open_borrowed(&endpoint, &SinkPolicy::default()).expect("open");
        drop(endpoint);
        for line in "one\ntwo".lines() {
            sink.send(line).expect("send");
        }
        sink.end().expect("end");

        let got: Vec<String> = (&mut rx).map(|item| item.expect("item")).collect();
        assert_eq!(got, ["one", "two"]);
    }

    /// The generated `Debug` puts no bound on the item type.
    #[test]
    fn an_endpoint_formats_whatever_its_item_type() {
        struct Opaque;
        let printed = format!("{:?}", StreamEndpoint::<Opaque>::default());
        assert!(printed.starts_with("StreamEndpoint"), "{printed}");
        assert!(!printed.contains("_phantom"), "{printed}");
    }

    /// The service's failure arrives after the call that started the stream already succeeded.
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

    /// No arrived call can report `EX_TRANSACTION_FAILED`; it is refused, the stream still ends.
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

    /// A consumer blocked in `recv` learns of a producer gone mid-stream only from the terminator.
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
                ..ReceiverPolicy::default()
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
        // The same object each time, so a service comparing it with its link sees one binder.
        assert_eq!(
            SIBinder::downgrade(&token.binder()),
            SIBinder::downgrade(&binder)
        );
    }

    /// A status `end_with` accepts but the reader refuses would end the stream on `BadValue`.
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

    /// Every field of AOSP's `Status` wire survives, which is why the arguments are three, not one.
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

    fn timed_policy(timeout: Duration) -> SinkPolicy {
        SinkPolicy {
            send_timeout: Some(timeout),
            ..SinkPolicy::default()
        }
    }

    /// Run `call` on its own thread: a wait the timeout failed to bound fails the test, not hangs.
    fn bounded<S: Send + 'static, R: Send + 'static>(
        subject: S,
        call: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> (R, Duration, S) {
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let mut subject = subject;
            let before = Instant::now();
            let outcome = call(&mut subject);
            let _ = done.send((outcome, before.elapsed(), subject));
        });
        watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the timeout must end the wait")
    }

    /// A ring holding nothing but `(512 - END_RESERVE) / 8` unread `i32` records.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    fn full_ring(policy: &SinkPolicy) -> (Sink<i32>, Receiver<i32>, i32) {
        let peer = Token::new().binder();
        let (rx, endpoint) = Receiver::<i32>::with_policy(
            &peer,
            &ReceiverPolicy {
                ring_bytes: 512,
                ..ReceiverPolicy::default()
            },
        )
        .expect("a receiver");
        assert!(endpoint.ring.is_some());
        let mut sink = Sink::<i32>::open_with(&endpoint, policy).expect("open");
        let fill = ((512 - END_RESERVE) / 8) as i32;
        for item in 0..fill {
            sink.send(&item).expect("room for it");
        }
        (sink, rx, fill)
    }

    /// A consumer nobody reads keeps the ring full; the timeout ends the send and costs no item.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_send_timeout_ends_a_send_on_a_full_ring_and_the_stream_goes_on() {
        let timeout = Duration::from_millis(50);
        let (sink, mut rx, fill) = full_ring(&timed_policy(timeout));
        let (sent, elapsed, mut sink) = bounded(sink, |sink| sink.send(&-1));
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert!(!sink.is_canceled());

        for expected in 0..fill {
            assert_eq!(rx.try_recv().expect("no error"), Some(expected));
        }
        assert_eq!(
            rx.try_recv().expect("no error"),
            None,
            "the item is not in the ring"
        );
        sink.send(&fill).expect("room again");
        sink.end().expect("end");
        assert_eq!(rx.recv().expect("the item").expect("ok"), fill);
        assert!(rx.recv().is_none());
        assert!(
            rx.end_status().expect("ended").is_ok(),
            "a clean end: nothing was lost"
        );
    }

    /// `Duration::ZERO` tries once; `send_all` stops at the item that timed out, keeping the rest.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_zero_send_timeout_never_waits_and_send_all_keeps_what_went_in() {
        let (sink, mut rx, fill) = full_ring(&timed_policy(Duration::ZERO));
        let (sent, _, sink) = bounded(sink, |sink| sink.send(&-1));
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));

        // Room for two records, then three to send: the third times out, the first two stay.
        assert_eq!(rx.try_recv().expect("no error"), Some(0));
        assert_eq!(rx.try_recv().expect("no error"), Some(1));
        let extra = [fill, fill + 1, fill + 2];
        let (sent, _, sink) = bounded(sink, move |sink| sink.send_all(extra.iter()));
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));
        sink.end().expect("end");
        let rest: Vec<i32> = (&mut rx).map(|item| item.expect("ok")).collect();
        assert_eq!(rest, (2..fill + 2).collect::<Vec<_>>());
        assert!(rx.end_status().expect("ended").is_ok());
    }

    /// An RPC-path pair over a local sink: one opening credit, one `i32` a batch.
    fn out_of_credit_pair(policy: SinkPolicy) -> (Sink<i32>, calls::Consumer<i32>) {
        let (rx, sink_binder) = calls::Consumer::<i32>::new(&ReceiverPolicy {
            credit_window: 1,
            max_opening: 4,
            ..ReceiverPolicy::default()
        });
        let endpoint = StreamEndpoint::with(None, sink_binder);
        let policy = SinkPolicy {
            initial_credits: 1,
            ..policy
        };
        let mut sink = Sink::<i32>::open_with(&endpoint, &policy).expect("open");
        sink.send(&0).expect("the opening credit");
        if sink.pending() > 0 {
            sink.flush().expect("the opening credit");
        }
        (sink, rx)
    }

    /// Out of credit: `send` gives the item back, and the stream runs on once a grant arrives.
    #[test]
    fn a_send_timeout_ends_a_wait_for_credit_and_the_item_is_not_queued() {
        let timeout = Duration::from_millis(50);
        let (sink, mut rx) = out_of_credit_pair(SinkPolicy {
            max_batch_bytes: 4,
            ..timed_policy(timeout)
        });
        let (sent, elapsed, mut sink) = bounded(sink, |sink| sink.send(&1));
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert_eq!(sink.pending(), 0, "the item was taken back out");

        // Window 1: draining the batch grants its credit back.
        assert_eq!(rx.recv().expect("item 0").expect("ok"), 0);
        sink.send(&1).expect("a grant came");
        sink.end().expect("end");
        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        assert!(rx.recv().is_none());
        assert!(
            rx.end_status().expect("ended").is_ok(),
            "a clean end: nothing was lost"
        );
    }

    /// A flush out of credit keeps its batch; `end` out of credit gives it up but still ends.
    #[test]
    fn a_flush_that_times_out_keeps_its_batch_and_an_end_that_does_still_ends() {
        let timeout = Duration::from_millis(50);
        let (mut sink, mut rx) = out_of_credit_pair(timed_policy(timeout));
        sink.send(&1).expect("queued");
        let (flushed, elapsed, mut sink) = bounded(sink, |sink| sink.flush());
        assert_eq!(flushed.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert_eq!(sink.pending(), 1, "the batch stays pending, whole");

        // Window 1: draining the batch grants its credit back.
        assert_eq!(rx.recv().expect("item 0").expect("ok"), 0);
        sink.flush().expect("a grant came");

        // No grant this time: nothing drains until `end` has returned.
        sink.send(&2).expect("queued");
        let (ended, elapsed, _) = bounded(Some(sink), |sink| sink.take().expect("sink").end());
        assert_eq!(ended.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        let failure = rx
            .recv()
            .expect("a terminator")
            .expect_err("not a clean end");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(
            failure
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the given-up batch is reported: {failure:?}"
        );
    }

    /// A binder cannot cross as bytes; it is refused before anything is written.
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

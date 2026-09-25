// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The RPC path: batches as `oneway` calls on `IStreamSink`, credit as
//! `oneway` calls back on `IStreamSource`.
//!
//! A socket holds its sender until the peer reads, so a `oneway` call
//! here is never refused for want of space the way a kernel `oneway` can
//! be; what paces the producer is the credit the consumer grants, and the
//! only failures are a session that ends and a frame that could not go
//! out whole. The types here are what [`Sink`](super::Sink) and
//! [`Receiver`](super::Receiver) wrap when the endpoint has no ring.

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

use super::generated::rsbinder::stream::IStreamSink::{BnStreamSink, BpStreamSink, IStreamSink};
// Only the hand-built request path names a transaction code; the typed
// proxy carries its own.
#[cfg(feature = "rpc")]
use super::generated::rsbinder::stream::IStreamSink::transactions as sink_transactions;
use super::generated::rsbinder::stream::IStreamSource::{
    BnStreamSource, BpStreamSource, IStreamSource,
};
#[cfg(feature = "tokio")]
use super::pool::on_pool;
use super::{
    status_from_fields, truncated_terminator, unlink_death, watch_death, ReceiverPolicy, SinkPolicy,
};

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
pub(super) struct Credit {
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

    /// Take one credit if one is there. For the producer's `Drop`, which
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

/// The object the consumer calls to grant credit — the binder half of
/// [`Credit`].
pub(super) struct SourceObject(pub(super) Arc<Credit>);

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

/// What became of the batches that left a producer, shared with the pool
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
/// arrived. A dead session, a session that refused the transaction and a
/// send deadline that expired before the frame went out whole all leave
/// nothing the peer could dispatch; any other failure may have come after
/// the frame was written, with the batch already on its way.
fn certainly_not_delivered(e: StatusCode) -> bool {
    matches!(
        e,
        StatusCode::FailedTransaction | StatusCode::DeadObject | StatusCode::TimedOut
    )
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
            Err(e) if certainly_not_delivered(e) => Outcome::Refused(e),
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

/// The producer over the RPC path: items are encoded into a pending
/// batch, which leaves as one `onBatch` call once it reaches the byte
/// threshold or is flushed, and each batch spends one credit.
pub(super) struct Producer<T: ?Sized> {
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

impl<T: Serialize + ?Sized> Producer<T> {
    /// Take the consumer's sink, introduce this producer with `onStart`,
    /// and watch the sink for death.
    pub(super) fn open(sink: &SIBinder, policy: &SinkPolicy) -> Result<Self> {
        let Some(declared) = i32::try_from(policy.initial_credits)
            .ok()
            .filter(|n| *n > 0)
        else {
            log::error!("Sink::open: the opening window must be 1..=i32::MAX batches");
            return Err(StatusCode::BadValue);
        };
        super::peer_caps(sink).require(crate::TransportCaps::CALLBACKS, "a streaming sink")?;
        let peer = resolve_peer::<dyn IStreamSink>(
            sink,
            <BpStreamSink as crate::Proxy>::descriptor(),
            "Sink::open",
        )?;
        let credit = Arc::new(Credit::default());
        credit.grant(policy.initial_credits);
        let source = BnStreamSource::new_binder(SourceObject(credit.clone())).as_binder();
        let death = watch_death(sink, SinkDeath(credit.clone()))?;
        // Before anything else can be sent, so the consumer knows where
        // to grant and whom to watch by the time the first batch lands.
        if let Err(e) = peer.on_start(&source, declared) {
            // No producer is built, so nothing else will undo the link.
            unlink_death(sink, &death);
            return Err(e);
        }
        Ok(Producer {
            peer: Arc::new(peer),
            credit,
            batch: Parcel::new_data_only(),
            count: 0,
            max_batch_bytes: policy.max_batch_bytes.max(1),
            ended: false,
            ledger: Arc::new(Ledger::default()),
            _source: source,
            sink: sink.clone(),
            death,
            _item: PhantomData,
        })
    }

    pub(super) fn send(&mut self, item: &T) -> Result<()> {
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

    pub(super) fn flush(&mut self) -> Result<()> {
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

    pub(super) fn terminate(
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

    /// The item is encoded now; the returned future sends the batch if it
    /// is full.
    #[cfg(feature = "tokio")]
    pub(super) fn send_async(
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

    #[cfg(feature = "tokio")]
    pub(super) async fn flush_async(&mut self) -> Result<()> {
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

    #[cfg(feature = "tokio")]
    pub(super) async fn terminate_async(
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

    /// Batches this producer may still send without waiting.
    #[cfg(test)]
    fn credits(&self) -> u64 {
        self.credit.lock().available
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
impl<T: ?Sized> Producer<T> {
    pub(super) fn is_canceled(&self) -> bool {
        self.credit.is_canceled()
    }

    pub(super) fn pending(&self) -> usize {
        self.count as usize
    }

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

impl<T: ?Sized> Drop for Producer<T> {
    /// Deliver what is queued and end the stream, without waiting for
    /// credit; see [`Sink`](super::Sink)'s `Drop`.
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
        let message = super::dropped_terminator(self.ledger.lost());
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
    /// The same binder, kept for the unlink in the consumer's `Drop`.
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

pub(super) struct Stream {
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
    /// first and would have `end_status` report success.
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

/// What an async consumer hands to [`on_pool`]: on the RPC stack a grant
/// waits for a free outgoing connection.
#[cfg(feature = "tokio")]
fn send_grant(grant: GrantToken) -> Result<()> {
    grant.send();
    Ok(())
}

/// Watches the producer — its source, and the peer the consumer was made
/// with — and ends the stream the way a terminator does, with
/// [`StatusCode::DeadObject`], so a consumer blocked in `recv` learns that
/// no batch is coming without a case of its own. `Weak`, because the
/// stream state owns this recipient.
pub(super) struct SourceDeath(std::sync::Weak<Stream>);

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
            // bytes and `count` disagree (`Consumer::batch_failed`).
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

/// The consumer over the RPC path: batches arrive on the sink object,
/// are decoded on the consuming thread, and each drained batch is paid
/// back with credit.
pub(super) struct Consumer<T> {
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

impl<T: Deserialize> Consumer<T> {
    /// A consumer and the sink binder for the endpoint.
    pub(super) fn new(policy: &ReceiverPolicy) -> (Self, SIBinder) {
        let stream = Arc::new(Stream::new(policy.max_opening.max(1)));
        let sink_binder = BnStreamSink::new_binder(SinkObject(stream.clone())).as_binder();
        let consumer = Consumer {
            stream,
            sink_binder: sink_binder.clone(),
            window: policy.credit_window.max(1),
            decoded: VecDeque::new(),
            finished: false,
            retry_armed: false,
        };
        (consumer, sink_binder)
    }

    pub(super) fn sink_binder(&self) -> SIBinder {
        self.sink_binder.clone()
    }

    /// A recipient for the peer's death: it ends the stream as the
    /// source's death does.
    pub(super) fn death_recipient(&self) -> SourceDeath {
        SourceDeath(Arc::downgrade(&self.stream))
    }

    pub(super) fn recv(&mut self) -> Option<BinderResult<T>> {
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

    pub(super) fn try_recv(&mut self) -> BinderResult<Option<T>> {
        self.retry_armed = true;
        match self.advance(false) {
            Some(done) => done.transpose(),
            None => Ok(None),
        }
    }

    pub(super) fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
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

    #[cfg(feature = "tokio")]
    pub(super) async fn recv_async(&mut self) -> Option<BinderResult<T>> {
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

impl<T> Consumer<T> {
    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn end_status(&self) -> Option<Status> {
        self.stream.lock().end.clone()
    }

    pub(super) fn cancel(&self) -> Result<()> {
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

    /// Whether the producer has introduced itself, for `Debug`.
    pub(super) fn started(&self) -> bool {
        self.stream.lock().source.is_some()
    }
}

impl<T> Drop for Consumer<T> {
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
                        super::generated::rsbinder::stream::IStreamSource::transactions::r#request,
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
                        super::generated::rsbinder::stream::IStreamSource::transactions::r#cancel,
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

    fn sink_policy(max_batch_bytes: usize, initial_credits: u32) -> SinkPolicy {
        SinkPolicy {
            max_batch_bytes,
            initial_credits,
            ..SinkPolicy::default()
        }
    }

    fn receiver_policy(credit_window: u32, max_opening: u32) -> ReceiverPolicy {
        ReceiverPolicy {
            credit_window,
            max_opening,
            ..ReceiverPolicy::default()
        }
    }

    /// A consumer with the grant window set, and a producer on its sink.
    /// The sink binder is local, so no transport is involved; the wire is
    /// `tests/stream_rpc.rs`.
    fn pair<T: Serialize + Deserialize>(
        window: u32,
        max_batch_bytes: usize,
        initial_credits: u32,
    ) -> (Producer<T>, Consumer<T>) {
        let (rx, sink_binder) = Consumer::<T>::new(&receiver_policy(window, window));
        let sink =
            Producer::<T>::open(&sink_binder, &sink_policy(max_batch_bytes, initial_credits))
                .expect("a local binder can always be called back");
        (sink, rx)
    }

    /// A consumer whose window and widest opening are the defaults.
    fn default_pair<T: Serialize + Deserialize>() -> (Producer<T>, Consumer<T>) {
        let policy = SinkPolicy::default();
        pair(
            ReceiverPolicy::default().credit_window,
            policy.max_batch_bytes,
            policy.initial_credits,
        )
    }

    #[derive(Default)]
    struct Recorded {
        /// How many `onBatch` calls to refuse; the next one clears it.
        refuse: AtomicUsize,
        /// Refuse the way a transport does when it cannot say whether the
        /// call arrived, rather than the way a session refuses one.
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

    /// A consumer whose producer has introduced itself with `credits`,
    /// the producer being a source this test drives by hand.
    fn started<T: Deserialize>(
        window: u32,
        max_opening: u32,
        credits: i32,
    ) -> (Consumer<T>, Strong<dyn IStreamSink>, Arc<SourceCalls>) {
        let calls = Arc::new(SourceCalls::default());
        let source = BnStreamSource::new_binder(RefusingSource(calls.clone())).as_binder();
        let (rx, sink_binder) = Consumer::<T>::new(&receiver_policy(window, max_opening));
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the consumer's own sink");
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
            RefusedByTheSession,
            FailedWithoutSayingWhich,
            DroppedUnsent,
            UnwoundUnsent,
            UnwoundInTheCall,
        }
        for way in [
            Way::Accepted,
            Way::RefusedByTheSession,
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
                Way::RefusedByTheSession => {
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

    /// A dead session, a refused transaction and an expired send deadline
    /// all leave a frame the peer cannot dispatch; any other failure may
    /// have come after the frame was written.
    #[test]
    fn what_a_failed_send_says_about_the_batch() {
        for (e, known) in [
            (StatusCode::FailedTransaction, true),
            (StatusCode::DeadObject, true),
            (StatusCode::TimedOut, true),
            (StatusCode::Unknown, false),
            (StatusCode::BadValue, false),
        ] {
            assert_eq!(certainly_not_delivered(e), known, "{e:?}");
        }
    }

    /// A send that cannot say whether its batch arrived ends the producer:
    /// the consumer holds the producer to its credit, and how much is left
    /// is no longer known. The terminator spends none, so it still goes out.
    #[test]
    fn a_send_that_cannot_say_whether_it_arrived_ends_the_producer() {
        let recorded = Arc::new(Recorded::default());
        recorded.unknown.store(true, Ordering::SeqCst);
        recorded.refuse.store(1, Ordering::SeqCst);
        let sink_binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        // Eight bytes to a batch: every second `i32` is what sends one.
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(8, 4)).expect("open");

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
        assert_eq!(
            sink.terminate(ExceptionCode::None as i32, 0, None).err(),
            Some(StatusCode::Unknown)
        );

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
                let settled = |rx: &Consumer<i32>, again: &str| {
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

    /// A pool that drops the terminator's task unrun says the task
    /// failed. The terminator went out all the same, from the token's
    /// `Drop`, and that is what the caller asked about.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_the_pool_drops_unrun_reports_the_send_not_the_pool() {
        let recorded = Arc::new(Recorded::default());
        let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        let sink = Producer::<i32>::open(&binder, &SinkPolicy::default()).expect("a local sink");

        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let handle = runtime.handle().clone();
        runtime.shutdown_background();

        assert_eq!(
            handle.block_on(sink.terminate_async(ExceptionCode::None as i32, 0, None)),
            Ok(())
        );
        assert_eq!(recorded.ends.load(Ordering::SeqCst), 1);
    }

    /// `terminate_async` polled where the pool cannot take the terminator:
    /// the stream is already marked ended by then, so `Drop` will not send
    /// one, and it must not be the case that nothing does.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_the_pool_cannot_take_still_terminates_the_stream() {
        let recorded = Arc::new(Recorded::default());
        let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        let sink = Producer::<i32>::open(&binder, &SinkPolicy::default()).expect("a local sink");

        // No runtime here, so `spawn_blocking` panics with the call in hand.
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut end = std::pin::pin!(sink.terminate_async(ExceptionCode::None as i32, 0, None));
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
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(2, 4));
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the consumer's own sink");
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
            // Sent back alive: dropping the consumer cancels too, and
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
        let (mut sink, mut rx) = default_pair::<i32>();
        let sent: Vec<i32> = (0..10).collect();
        for item in &sent {
            sink.send(item).expect("send");
        }
        sink.terminate(ExceptionCode::None as i32, 0, None)
            .expect("end");

        let mut got = Vec::new();
        while let Some(item) = rx.recv() {
            got.push(item.expect("item"));
        }
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
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let (progress, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            for item in 0..4i32 {
                if sink.send(&item).is_err() {
                    return;
                }
                let _ = progress.send(item);
            }
            let _ = sink.terminate(ExceptionCode::None as i32, 0, None);
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

        let mut got = Vec::new();
        while let Some(item) = rx.recv() {
            got.push(item.expect("item"));
        }
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
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(2, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

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
            assert_eq!(rx.recv().expect("an item").expect("item"), expected);
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

    /// A producer whose opening window is smaller than the consumer's
    /// grant threshold runs out of credit before the threshold is ever
    /// reached. The consumer has to grant what it owes before it waits,
    /// or both ends wait on each other.
    #[test]
    fn an_opening_window_below_the_grant_threshold_does_not_stall() {
        // Threshold is half of eight; the producer opens with one.
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(8, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let mut sent = Ok(());
            for item in 0..20i32 {
                sent = sink.send(&item);
                if sent.is_err() {
                    break;
                }
            }
            let _ =
                done.send(sent.and_then(|()| sink.terminate(ExceptionCode::None as i32, 0, None)));
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
    fn a_consumer_dropped_before_the_stream_starts_cancels_it_on_arrival() {
        let (rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        drop(rx);

        let mut sink = Producer::<i32>::open(&sink_binder, &SinkPolicy::default())
            .expect("the sink object is still there");
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
                let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
                let mut sink =
                    Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");
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
                    sink.terminate_async(ExceptionCode::None as i32, 0, None)
                        .await
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
        let (rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

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
    fn a_dropped_producer_flushes_and_reports_that_it_never_ended() {
        let (mut sink, mut rx) = default_pair::<i32>();
        sink.send(&1).expect("send");
        sink.send(&2).expect("send");
        assert_eq!(sink.pending(), 2, "neither item filled a 16 KB batch");
        drop(sink);

        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        assert_eq!(rx.recv().expect("item 2").expect("ok"), 2);
        let failure = rx.recv().expect("a terminator").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(rx.recv().is_none(), "the stream is over");
    }

    /// The same drop with no credit left: the queued item cannot go out,
    /// and waiting for a grant would park whichever thread is dropping.
    #[test]
    fn a_dropped_producer_with_no_credit_still_ends_the_stream() {
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        // Eight bytes to a batch is two `i32`s, and one opening credit.
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(8, 1)).expect("open");

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

        assert_eq!(rx.recv().expect("item 0").expect("ok"), 0);
        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        let failure = rx.recv().expect("a terminator").expect_err("an error");
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
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 4)).expect("open");

        sink.send(&0).expect_err("the first batch is refused");
        // The failure cost that item and nothing else: the stream is
        // still usable, as `send`'s rustdoc says.
        sink.send(&1).expect("send");
        sink.terminate(ExceptionCode::None as i32, 0, None)
            .expect("end");

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

    #[test]
    fn an_opening_window_of_nothing_is_refused() {
        let (_rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        assert_eq!(
            Producer::<i32>::open(&sink_binder, &sink_policy(16, 0)).err(),
            Some(StatusCode::BadValue)
        );
    }

    #[test]
    fn a_binder_that_is_not_a_sink_is_refused() {
        let not_a_sink =
            BnStreamSource::new_binder(SourceObject(Arc::new(Credit::default()))).as_binder();
        assert_eq!(
            Producer::<i32>::open(&not_a_sink, &SinkPolicy::default()).err(),
            Some(StatusCode::BadType)
        );
    }

    /// A batch whose stated count does not match its bytes is refused
    /// rather than guessed at, and the items decoded before the mismatch
    /// are not handed out.
    #[test]
    fn a_batch_that_claims_more_items_than_it_carries_is_refused() {
        let (mut rx, sink, _calls) = started::<i32>(4, 4, 4);

        sink.r#onBatch(&[1, 0, 0, 0], 2).expect("onBatch");
        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");

        assert!(
            rx.recv().expect("a report").is_err(),
            "one i32 cannot be two items"
        );
        assert!(
            rx.recv().is_none(),
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
        let (mut rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the consumer's own sink");
        sink.r#onStart(&source, 4).expect("onStart");

        assert!(sink.r#onBatch(&[1, 0, 0, 0], -1).is_err());
        assert!(
            credit.is_canceled(),
            "no more credit is coming, so only a cancel releases the producer"
        );
        sink.r#onEnd(ExceptionCode::None as i32, 0, None)
            .expect("onEnd");

        assert_eq!(
            rx.recv()
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
        assert_eq!(rx.recv().expect("a batch").expect("an item"), 1);
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
        let (mut rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the consumer's own sink");

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
            let (mut rx, _sink, calls) = started::<i32>(4, 4, credits);
            assert_eq!(
                rx.try_recv().expect_err("an error").exception_code(),
                ExceptionCode::IllegalArgument,
                "{credits}"
            );
            assert_eq!(calls.canceled.load(Ordering::SeqCst), 1, "{credits}");
        }

        let (rx, _sink, calls) = started::<i32>(4, 8, 5);
        assert!(
            rx.end_status().is_none(),
            "a consumer set up for it takes it"
        );
        assert_eq!(calls.canceled.load(Ordering::SeqCst), 0);

        // The producer's side of the same refusal.
        let (_rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        let sink = Producer::<i32>::open(&sink_binder, &sink_policy(16, 5))
            .expect("`onStart` is oneway, so the refusal is not an error here");
        assert!(sink.is_canceled());
    }

    /// A second `onStart` is somebody else's mistake: whatever it states,
    /// it must not end the stream that is running.
    #[test]
    fn a_second_on_start_cannot_end_the_stream() {
        let (rx, sink, _calls) = started::<i32>(4, 4, 4);
        let other = Arc::new(SourceCalls::default());
        let intruder = BnStreamSource::new_binder(RefusingSource(other.clone())).as_binder();

        sink.r#onStart(&intruder, i32::MAX).expect("onStart");

        assert!(rx.end_status().is_none());
        assert_eq!(other.canceled.load(Ordering::SeqCst), 1);

        // Nor one whose source is not a source at all.
        let (_other_rx, not_a_source) = Consumer::<i32>::new(&ReceiverPolicy::default());
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

        assert_eq!(rx.recv().expect("a batch").expect("an item"), 1);
        assert_eq!(
            calls.attempts.load(Ordering::SeqCst),
            0,
            "one owed, one still queued"
        );
        assert_eq!(rx.recv().expect("a batch").expect("an item"), 2);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 2);
    }

    /// A producer can make every grant fail while its process stays
    /// alive, so no death link fires. However often the consumer retries,
    /// what the producer may send does not grow: the bound on the queue
    /// is the opening window.
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
            // Sent back alive: dropping the consumer cancels too.
            let _ = done.send((first, second, rx));
        });
        let (first, second, _rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("neither end could otherwise move");
        assert_eq!(first.expect("an item").expect("ok"), 1);
        assert!(second.expect("a report").is_err());
        assert_eq!(calls.canceled.load(Ordering::SeqCst), 1);
    }
}

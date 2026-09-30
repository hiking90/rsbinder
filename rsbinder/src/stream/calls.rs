// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The RPC path: batches as `oneway` calls on `IStreamSink`, credit as `oneway` calls back on
//! `IStreamSource`; what [`Sink`](super::Sink) and [`Receiver`](super::Receiver) wrap without a ring.
//! A socket send is never refused for want of space as a kernel `oneway` is, so credit alone paces.

use std::collections::VecDeque;
use std::marker::PhantomData;
#[cfg(feature = "tokio")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::binder::{FromIBinder, Interface, Strong};
use crate::error::{Result, StatusCode};
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, Serialize};
use crate::status::{BinderResult, ExceptionCode, Status};
use crate::SIBinder;

use super::generated::rsbinder::stream::IStreamSink::{BnStreamSink, BpStreamSink, IStreamSink};
// Only the hand-built request path names a transaction code; the typed proxy has its own.
#[cfg(feature = "rpc")]
use super::generated::rsbinder::stream::IStreamSink::transactions as sink_transactions;
use super::generated::rsbinder::stream::IStreamSource::{
    BnStreamSource, BpStreamSource, IStreamSource,
};
#[cfg(feature = "tokio")]
use super::pool::on_pool;
use super::{
    status_from_fields, truncated_terminator, unlink_death, watch_death, PingPolicy,
    ReceiverPolicy, SinkPolicy,
};

// ---- Pinging a quiet peer (plan 2-24 D9) ----

/// Floor under a third of a tiny reply deadline, so a sub-3 ms deadline cannot spin the wait.
#[cfg(feature = "rpc")]
const MIN_PING_INTERVAL: Duration = Duration::from_millis(1);

/// Whom a wait pings once it has gone `every` without a wake.
struct Ping {
    peer: SIBinder,
    every: Duration,
}

impl Ping {
    /// `None` when `policy` is off, `peer` is no RPC proxy, or its session has no deadline.
    fn to(peer: &SIBinder, policy: PingPolicy) -> Option<Ping> {
        if policy == PingPolicy::Off {
            return None;
        }
        #[cfg(feature = "rpc")]
        if let Some(proxy) = (**peer).as_any().downcast_ref::<crate::rpc::RpcProxy>() {
            // Read per wait: a server sets a session's deadline only after the session exists.
            let every = (proxy.session_timeout()? / 3).max(MIN_PING_INTERVAL);
            return Some(Ping {
                peer: peer.clone(),
                every,
            });
        }
        #[cfg(not(feature = "rpc"))]
        let _ = peer;
        None
    }

    /// Unanswered within the reply deadline, it ends the session, and the death link the stream.
    fn send(&self) {
        match self.peer.ping_binder() {
            Ok(()) | Err(StatusCode::DeadObject) => {}
            Err(e) => log::warn!("stream: pinging the peer failed: {e:?}"),
        }
    }
}

/// Wait on `cv` while `blocked` until `deadline` or `ping.every`; the ping goes out unlocked.
fn park<S>(
    mutex: &Mutex<S>,
    cv: &Condvar,
    blocked: impl Fn(&S) -> bool,
    deadline: Option<Instant>,
    ping: Option<&Ping>,
) {
    let quiet_until = ping.and_then(|ping| Instant::now().checked_add(ping.every));
    // A caller's deadline that comes first ends the wait without a ping.
    let pings = quiet_until.is_some_and(|quiet| deadline.is_none_or(|d| quiet <= d));
    let wake_at = if pings { quiet_until } else { deadline };
    let mut state = mutex.lock().unwrap_or_else(|e| e.into_inner());
    while blocked(&state) {
        let Some(at) = wake_at else {
            state = cv.wait(state).unwrap_or_else(|e| e.into_inner());
            continue;
        };
        let left = at.saturating_duration_since(Instant::now());
        if left.is_zero() {
            drop(state);
            if let Some(ping) = ping.filter(|_| pings) {
                ping.send();
            }
            return;
        }
        state = cv
            .wait_timeout(state, left)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}

/// Dropped with its future, it ends that future's pooled [`park`] before it can ping.
#[cfg(feature = "tokio")]
struct Abandon<'a, S> {
    gone: Arc<AtomicBool>,
    mutex: &'a Mutex<S>,
    cv: &'a Condvar,
}

#[cfg(feature = "tokio")]
impl<'a, S> Abandon<'a, S> {
    fn new(mutex: &'a Mutex<S>, cv: &'a Condvar) -> Self {
        Abandon {
            gone: Arc::default(),
            mutex,
            cv,
        }
    }

    /// Set once the future is dropped; the pooled wait's `blocked` reads it.
    fn gone(&self) -> Arc<AtomicBool> {
        self.gone.clone()
    }
}

#[cfg(feature = "tokio")]
impl<S> Drop for Abandon<'_, S> {
    fn drop(&mut self) {
        self.gone.store(true, Ordering::Release);
        // Locked once, so the store cannot fall between the wait's check and its sleep.
        drop(self.mutex.lock().unwrap_or_else(|e| e.into_inner()));
        self.cv.notify_all();
    }
}

// ---- Shared credit state (producer side) ----

/// What the producer waits on and the source object's binder feeds.
#[derive(Default)]
struct CreditState {
    /// Batches the producer may still send.
    available: u64,
    /// Highest running total granted; a total not above it is a repeat and adds nothing.
    granted_total: i64,
    /// The consumer asked for no more. Latched: a cancel is never undone.
    canceled: bool,
    /// Consumer process gone. Latched; wins over `canceled` as the more specific answer.
    dead: bool,
}

#[derive(Default)]
pub(super) struct Credit {
    state: Mutex<CreditState>,
    /// Woken by a grant, a cancel and the consumer's death.
    wake: Condvar,
    /// The same three, for a producer suspended in `wait_credit_async`.
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

    /// Only the part above the highest total seen is new, so a repeated grant is harmless.
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

    /// One step of taking a credit: `None` if the caller has to wait.
    fn poll_credit(state: &mut CreditState) -> Option<Result<()>> {
        if let Err(e) = Self::latch(state) {
            return Some(Err(e));
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

    /// The latched death or cancel as an error; `Ok` while neither.
    fn latched(&self) -> Result<()> {
        Self::latch(&self.lock())
    }

    fn latch(state: &CreditState) -> Result<()> {
        if state.dead {
            return Err(StatusCode::DeadObject);
        }
        if state.canceled {
            return Err(StatusCode::InvalidOperation);
        }
        Ok(())
    }

    /// Non-blocking, for the producer's `Drop`, which must not park an unwinding thread.
    fn try_credit(&self) -> bool {
        let mut state = self.lock();
        if state.dead || state.canceled || state.available == 0 {
            return false;
        }
        state.available -= 1;
        true
    }

    /// No credit to take and nothing latched: what a credit wait waits out.
    fn starved(state: &CreditState) -> bool {
        Self::latch(state).is_ok() && state.available == 0
    }

    /// Death ends the wait too: with nothing in flight, nothing else tells a parked producer.
    fn wait_credit(
        &self,
        deadline: Option<Instant>,
        sink: &SIBinder,
        ping: PingPolicy,
    ) -> Result<()> {
        loop {
            if let Some(answer) = Self::poll_credit(&mut self.lock()) {
                return answer;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(StatusCode::TimedOut);
            }
            let ping = Ping::to(sink, ping);
            park(
                &self.state,
                &self.wake,
                Self::starved,
                deadline,
                ping.as_ref(),
            );
        }
    }

    /// [`wait_credit`](Self::wait_credit) that suspends the task instead of parking the thread.
    #[cfg(feature = "tokio")]
    async fn wait_credit_async(
        self: &Arc<Self>,
        deadline: Option<Instant>,
        sink: &SIBinder,
        ping: PingPolicy,
    ) -> Result<()> {
        loop {
            // Created first: a `Notified` sees every `notify_waiters` from its creation on.
            let notified = self.notify.notified();
            if let Some(answer) = Self::poll_credit(&mut self.lock()) {
                return answer;
            }
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Err(StatusCode::TimedOut);
            }
            let ping = Ping::to(sink, ping);
            if deadline.is_none() && ping.is_none() {
                notified.await;
                continue;
            }
            let abandon = Abandon::new(&self.state, &self.wake);
            let gone = abandon.gone();
            // No `tokio/time` to suspend against, so a bounded wait is a condvar on the pool.
            let pooled = on_pool(self.clone(), move |credit| {
                park(
                    &credit.state,
                    &credit.wake,
                    |s| Self::starved(s) && !gone.load(Ordering::Acquire),
                    deadline,
                    ping.as_ref(),
                );
                Ok(())
            })
            .await;
            drop(abandon);
            if let Err(e) = pooled {
                // The pool never ran the wait, so nothing else ends it.
                return Self::poll_credit(&mut self.lock()).unwrap_or(Err(e));
            }
        }
    }
}

/// Watches the consumer's sink so a producer parked on `wait_credit` learns nobody is there.
struct SinkDeath(Arc<Credit>);

impl crate::DeathRecipient for SinkDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        self.0.mark_dead();
    }
}

/// The object the consumer calls to grant credit — the binder half of [`Credit`].
pub(super) struct SourceObject(pub(super) Arc<Credit>);

impl Interface for SourceObject {}

impl IStreamSource for SourceObject {
    fn r#request(&self, total: i64) -> BinderResult<()> {
        if total <= 0 {
            // `oneway`: a non-positive total cannot be refused, only logged.
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

// ---- Talking to a binder that may not know its own interface yet ----

/// Resolved once, driven by hand: a typed cast stamps a descriptor on a shared wire proxy for good.
enum Peer<I: FromIBinder + ?Sized> {
    Typed(Strong<I>),
    #[cfg(feature = "rpc")]
    Unstamped(SIBinder),
}

/// Resolve `binder` against `I`, refusing a binder that states a different interface.
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

// ---- Producer ----

#[derive(Default)]
struct LedgerState {
    /// Items that left the pending batch and never arrived; only the terminator can report them.
    lost: i32,
    /// A send that failed with nobody told yet; the next call returns it.
    unreported: Option<StatusCode>,
    /// A batch is on its way; nothing may be sent past it, or it would arrive out of order.
    in_transit: bool,
    /// A failed send's delivery is unknown, so remaining credit is too; nothing more is sent.
    broken: Option<StatusCode>,
}

/// Batch outcomes, written by the pool task: dropping a `*_async` future detaches, not cancels it.
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

/// `e` left the peer nothing to dispatch: refused, dead, or unsent (`TimedOut`, `WouldBlock`).
fn certainly_not_delivered(e: StatusCode) -> bool {
    matches!(
        e,
        StatusCode::FailedTransaction
            | StatusCode::DeadObject
            | StatusCode::TimedOut
            | StatusCode::WouldBlock
    )
}

/// Batch and credit; `Drop`, the only exit, refunds an undelivered batch and counts it lost.
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
            // The consumer never grants this credit back; the items are counted lost, not resent.
            self.credit.grant(1);
        }
        {
            let mut state = self.ledger.lock();
            if let Some(e) = failure {
                state.lost = state.lost.saturating_add(self.count);
                state.unreported = Some(e);
                // Delivery unknown: the credit can neither be given back nor kept safely.
                if !not_delivered {
                    state.broken = Some(e);
                }
            }
            // Same critical section: a waiter that sees the batch gone sees its outcome.
            state.in_transit = false;
        }
        self.ledger.idle.notify_all();
        #[cfg(feature = "tokio")]
        self.ledger.notify.notify_waiters();
    }
}

/// Pool-bound terminator; `Drop` sends it if not run, or the consumer blocks with both ends alive.
#[cfg(feature = "tokio")]
struct TerminatorInTransit {
    peer: Arc<Peer<dyn IStreamSink>>,
    exception: i32,
    service_specific: i32,
    message: Option<String>,
    /// The send's result, wherever made: the pool's own answer says only whether the task ran.
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

/// RPC-path producer: a batch goes out as one `onBatch` for one credit, when full or flushed.
pub(super) struct Producer<T: ?Sized> {
    /// Shared so an async send can move it onto the blocking pool.
    peer: Arc<Peer<dyn IStreamSink>>,
    credit: Arc<Credit>,
    /// Items encoded so far, awaiting a batch send.
    batch: Parcel,
    count: i32,
    max_batch_bytes: usize,
    /// Set once a terminator has gone out, so `Drop` does not send a second one.
    ended: bool,
    /// What became of the batches that left; shared with a pool task.
    ledger: Arc<Ledger>,
    /// Lives as long as the producer; the consumer got its own reference in `onStart`.
    _source: SIBinder,
    /// The consumer's sink, kept for the unlink in `Drop` and pinged by a quiet credit wait.
    sink: SIBinder,
    ping: PingPolicy,
    /// Holds the death link on the sink; the binder keeps only a `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    _item: PhantomData<fn(&T)>,
}

impl<T: Serialize + ?Sized> Producer<T> {
    /// Introduce this producer to the sink with `onStart` and watch the sink for death.
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
        // First, so the consumer knows where to grant and whom to watch before any batch lands.
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
            ping: policy.ping,
            death,
            _item: PhantomData,
        })
    }

    pub(super) fn send(&mut self, item: &T, deadline: Option<Instant>) -> Result<()> {
        // Before the item is queued: nothing will send it.
        self.usable()?;
        self.credit.latched()?;
        let mark = self.batch.data_size();
        if !self.encode(item)? {
            return Ok(());
        }
        // As in `flush`, but a credit timeout takes this item back out: `TimedOut` = not queued.
        self.ledger.wait_idle();
        self.reported()?;
        if let Err(e) = self.credit.wait_credit(deadline, &self.sink, self.ping) {
            return self.unqueue_on_timeout(e, mark);
        }
        self.send_with_credit()
    }

    /// The error that broke the stream, if one has: see `LedgerState::broken`.
    fn usable(&self) -> Result<()> {
        self.ledger.broken().map_or(Ok(()), Err)
    }

    /// Queue one item; `true` when the batch has reached its threshold and should go out.
    fn encode(&mut self, item: &T) -> Result<bool> {
        let mark = self.batch.data_size();
        if let Err(e) = self.batch.write(item) {
            // A failed write may leave part of the item; cut back so accepted items stay intact.
            self.truncate_batch(mark)?;
            return Err(e);
        }
        self.count += 1;
        Ok(self.batch.data_size() >= self.max_batch_bytes)
    }

    /// A credit timeout leaves the pending batch as it was, for a later flush to send.
    pub(super) fn flush(&mut self, deadline: Option<Instant>) -> Result<()> {
        self.usable()?;
        if self.count == 0 {
            // Without blocking: a failure a dropped future's batch has already settled.
            return self.reported();
        }
        // A batch a dropped future left on the pool goes out first, and its failure is this call's.
        self.ledger.wait_idle();
        self.reported()?;
        self.credit.wait_credit(deadline, &self.sink, self.ping)?;
        self.send_with_credit()
    }

    /// The caller holds a credit: send the pending batch with it.
    fn send_with_credit(&mut self) -> Result<()> {
        if let Some(batch) = self.take_pending() {
            batch.send(&self.peer);
        }
        self.reported()
    }

    /// `e` from the credit wait after the item at `mark` was queued; a timeout un-queues it.
    fn unqueue_on_timeout(&mut self, e: StatusCode, mark: usize) -> Result<()> {
        if e == StatusCode::TimedOut {
            self.truncate_batch(mark)?;
            self.count -= 1;
        }
        Err(e)
    }

    /// The last batch's failure, reported once; a broken stream reports it on every call.
    fn reported(&self) -> Result<()> {
        let unreported = self.ledger.take_unreported();
        unreported.or(self.ledger.broken()).map_or(Ok(()), Err)
    }

    pub(super) fn terminate(
        mut self,
        deadline: Option<Instant>,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        // `Drop` runs next and must not send a second terminator.
        self.ended = true;
        // Don't overtake a pool batch; take its failure so the flush still sends what is queued.
        self.ledger.wait_idle();
        let stale = self.ledger.take_unreported();
        let failed = match (self.flush(deadline), stale) {
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
            // After a cancel no loss or flush failure is reported: the consumer asked for no more.
            _ if self.credit.is_canceled() => {
                self.peer.on_end(exception, service_specific, message)
            }
            // The consumer still blocks in `recv`: end it anyway, report the earlier flush error.
            Some(e) => {
                if let Err(end) = self.send_terminator(exception, service_specific, message) {
                    log::warn!("stream: the terminator could not be delivered: {end:?}");
                }
                Err(e)
            }
            // Caller's terminator, unless an earlier send lost items, which nothing else reports.
            None => self.send_terminator(exception, service_specific, message),
        }
    }

    /// The caller's terminator, or the one saying items went missing when any did.
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

    /// The item is encoded now; the returned future sends the batch if it is full.
    #[cfg(feature = "tokio")]
    pub(super) fn send_async(
        &mut self,
        item: &T,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let mark = self.batch.data_size();
        let encoded = self
            .usable()
            .and_then(|()| self.credit.latched())
            .and_then(|()| self.encode(item));
        async move {
            let deadline = super::call_deadline(timeout);
            if !encoded? {
                return Ok(());
            }
            // As in `send`.
            self.ledger.idle_async().await;
            self.reported()?;
            let waited = self
                .credit
                .wait_credit_async(deadline, &self.sink, self.ping)
                .await;
            if let Err(e) = waited {
                return self.unqueue_on_timeout(e, mark);
            }
            self.send_with_credit_async().await
        }
    }

    #[cfg(feature = "tokio")]
    pub(super) async fn flush_async(&mut self, deadline: Option<Instant>) -> Result<()> {
        self.usable()?;
        if self.count == 0 {
            return self.reported();
        }
        // As in `flush`.
        self.ledger.idle_async().await;
        self.reported()?;
        self.credit
            .wait_credit_async(deadline, &self.sink, self.ping)
            .await?;
        self.send_with_credit_async().await
    }

    /// [`send_with_credit`](Self::send_with_credit), the send made from the pool.
    #[cfg(feature = "tokio")]
    async fn send_with_credit_async(&mut self) -> Result<()> {
        if let Some(batch) = self.take_pending() {
            let peer = self.peer.clone();
            // `batch` records its outcome itself: the future may drop, the task may never run.
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
        deadline: Option<Instant>,
        exception: i32,
        service_specific: i32,
        message: Option<String>,
    ) -> Result<()> {
        // As in `terminate`.
        self.ledger.idle_async().await;
        // Put back if the future is dropped at the await below, so `Drop` still reports it.
        struct Stale(Arc<Ledger>, Option<StatusCode>);
        impl Stale {
            fn take(mut self) -> Option<StatusCode> {
                self.1.take()
            }
        }
        impl Drop for Stale {
            fn drop(&mut self) {
                if let Some(e) = self.1.take() {
                    self.0.lock().unreported.get_or_insert(e);
                }
            }
        }
        let stale = Stale(self.ledger.clone(), self.ledger.take_unreported());
        let flushed = self.flush_async(deadline).await;
        let failed = match (flushed, stale.take()) {
            (Err(e), _) => {
                // Nothing will send what that flush left queued.
                self.abandon_pending();
                Some(e)
            }
            (Ok(()), stale) => stale,
        };
        // As in `terminate`: only a dead consumer leaves nobody to send the terminator to.
        if let Some(StatusCode::DeadObject) = failed {
            self.ended = true;
            return Err(StatusCode::DeadObject);
        }
        let canceled = self.credit.is_canceled();
        // As in `terminate`: cancel keeps the caller's end; losses are reported here or nowhere.
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
        // Set only now: a future dropped in the awaits above must leave `Drop` a stream to end.
        self.ended = true;
        let pooled = on_pool(terminator, |terminator| {
            terminator.send();
            Ok(())
        })
        .await;
        // The token's answer first: if the pool never ran it, its `Drop` still sent the end.
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

    /// Rebuilt, as `Parcel` has no truncate and moving the cursor back would not shorten it.
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

// Only encoding needs `Serialize`; `Drop` must match the struct's declared bounds exactly.
impl<T: ?Sized> Producer<T> {
    pub(super) fn is_canceled(&self) -> bool {
        self.credit.is_canceled()
    }

    pub(super) fn pending(&self) -> usize {
        self.count as usize
    }

    /// Take the pending batch with the caller's credit; `None`: failure in the ledger, credit back.
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

    /// Give up on what is queued: counted lost and taken out, so nothing counts it twice.
    fn abandon_pending(&mut self) {
        self.ledger.add_lost(std::mem::replace(&mut self.count, 0));
        self.batch = Parcel::new_data_only();
    }
}

impl<T: ?Sized> Drop for Producer<T> {
    /// Deliver what is queued and end the stream without waiting for credit.
    fn drop(&mut self) {
        // Unlink regardless: a dropped recipient leaves a dead `Weak` in the sink's list for good.
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
        // Everything lost since the start: nothing after this terminator can report it.
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

/// Must match the generated proxy's `oneway` flags (`rsbinder-aidl/src/generator.rs`).
#[cfg(feature = "rpc")]
const ONEWAY_FLAGS: crate::TransactionFlags =
    crate::FLAG_ONEWAY | crate::FLAG_CLEAR_BUF | crate::FLAG_PRIVATE_LOCAL;

// ---- Consumer ----

/// What the sink receives and the consumer asks for, shared by binder threads and the consumer.
#[derive(Default)]
struct StreamState {
    /// Raw batches, decoded on the consumer's thread so a malformed one surfaces as its error.
    batches: VecDeque<(Vec<u8>, i32)>,
    /// Set once by `onEnd`, producer death, or a failed grant with nothing to prompt another.
    end: Option<Status>,
    /// Where to grant and cancel; `None` until `onStart` arrives.
    source: Option<Arc<Peer<dyn IStreamSource>>>,
    /// The same binder, kept for the unlink in the consumer's `Drop`.
    source_binder: Option<SIBinder>,
    /// Holds the death link on the source; the binder keeps only a `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    /// The consumer wants no more; an `onStart` arriving after this is cancelled on the spot.
    canceled: bool,
    /// The receiver is gone, so nothing will drain a batch again.
    closed: bool,
    /// The opening window the producer stated in `onStart`.
    declared: u32,
    /// Batches accepted into the queue.
    received: u64,
    /// Batches taken out; the running total a grant carries, read by the wait with the two below.
    drained: u64,
    /// Highest total a grant tried; a failed `oneway` may have arrived, so the total counts once.
    announced: u64,
    /// The highest total a grant was sent with.
    granted: u64,
    /// Last grant failed; retry waits for a batch or the next consumer call instead of spinning.
    grant_failed: bool,
    /// Told, under the lock, each time the consumer's wait is about to sleep.
    #[cfg(test)]
    parked: Option<std::sync::mpsc::Sender<()>>,
}

impl StreamState {
    /// Batches drained that no grant has been sent for.
    fn owed(&self) -> u64 {
        self.drained - self.granted
    }

    /// The most batches the producer can have credit for; one beyond is sending without credit.
    fn issued(&self) -> u64 {
        self.declared as u64 + self.announced
    }

    /// The producer has used everything it is certain to have been given.
    fn producer_may_be_starved(&self) -> bool {
        self.received >= self.declared as u64 + self.granted
    }

    /// Empty queue, maybe-starved producer: a grant failing now has nothing to prompt a retry.
    fn still_last_chance(&self) -> bool {
        self.batches.is_empty() && self.producer_may_be_starved()
    }

    /// The consumer parks only with nothing queued, no end, and no grant to send.
    fn must_wait(&self) -> bool {
        if !self.batches.is_empty() || self.end.is_some() {
            return false;
        }
        // No grant owed, or the last one failed and waits for a batch or the next call.
        self.owed() == 0 || self.grant_failed
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

    /// End a stream the consumer gave up on; `overriding` replaces an end already recorded.
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

    /// End the stream for a refused `onStart` unless a source is kept; true = cancel `source`.
    fn refuse_start(&self, source: &SIBinder, status: Status) -> bool {
        {
            let mut state = self.lock();
            if let Some(kept) = &state.source_binder {
                return *kept != SIBinder::downgrade(source);
            }
            state.end.get_or_insert(status);
            state.canceled = true;
        }
        self.wake();
        true
    }
}

/// A failed stream's owed cancel, sent on drop; a value so async code can move it off-executor.
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

/// One grant: the running total to announce. A lost one leaves `granted` as it was, owing nothing.
struct GrantToken {
    stream: Arc<Stream>,
    source: Arc<Peer<dyn IStreamSource>>,
    total: u64,
    /// No retry would come (consumer about to block, producer maybe starved): a failure ends it.
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
            // Re-checked: a batch landed since then is drained next, which retries the grant.
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

/// Run on [`on_pool`]: on the RPC stack a grant waits for a free outgoing connection.
#[cfg(feature = "tokio")]
fn send_grant(grant: GrantToken) -> Result<()> {
    grant.send();
    Ok(())
}

/// Ends the stream with `DeadObject` when the producer dies; `Weak` since the stream owns it.
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
        // Early answer to a second `onStart`; `refuse_start` and the keep below settle the race.
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
                // Without a source the stream stalls; `oneway`, so only the consumer can be told.
                self.0.refuse_start(source, Status::from(code));
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
            let status = Status::from((ExceptionCode::IllegalArgument, message.as_str()));
            if self.0.refuse_start(source, status) {
                // The producer would spend its stated credit on discarded batches, then park.
                let _ = peer.cancel();
            }
            return Ok(());
        };
        // Linked before taking the lock: no binder call under it.
        let death = match watch_death(source, SourceDeath(Arc::downgrade(&self.0))) {
            Ok(death) => death,
            Err(code) => {
                if self.0.refuse_start(source, Status::from(code)) {
                    let _ = peer.cancel();
                }
                return Ok(());
            }
        };
        let cancel_now;
        let mut kept = false;
        {
            let mut state = self.0.lock();
            if let Some(kept_source) = &state.source_binder {
                log::warn!("stream: ignoring a second onStart");
                // Only another producer is cancelled; `WIBinder` survives a proxy's resurrection.
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
        if kept {
            // A wait begun before now had nobody to ping; woken, it looks the source up.
            self.0.wake();
        }
        if !kept {
            // Nothing owns this link now; dropping the recipient would only make it inert.
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
            // oneway drops the returned error; ending the stream is what reaches the consumer.
            self.0.fail(status.clone(), false).send();
            return Err(status);
        }
        {
            let mut state = self.0.lock();
            // Past the terminator or the receiver nothing is read; dropping beats an unread queue.
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
        // Credit bounds this queue; a batch without it (even pre-`onStart`) would grow it forever.
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
            // Unreadable: the stream still stops, with the decode failure as its reason.
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

/// RPC-path consumer: batches land on the sink, decode on the consuming thread, repaid in credit.
pub(super) struct Consumer<T> {
    stream: Arc<Stream>,
    sink_binder: SIBinder,
    window: u32,
    /// Items decoded from the batch being drained.
    decoded: VecDeque<T>,
    /// Set once the end or a decode failure is reported: yielded once, then reads as finished.
    finished: bool,
    /// This call has not yet retried a grant that failed; it gets one try.
    retry_armed: bool,
    ping: PingPolicy,
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
            ping: policy.ping,
        };
        (consumer, sink_binder)
    }

    pub(super) fn sink_binder(&self) -> SIBinder {
        self.sink_binder.clone()
    }

    /// A recipient for the peer's death that ends the stream as the source's death does.
    pub(super) fn death_recipient(&self) -> SourceDeath {
        SourceDeath(Arc::downgrade(&self.stream))
    }

    pub(super) fn recv(&mut self) -> Option<BinderResult<T>> {
        self.retry_armed = true;
        loop {
            if let Some(done) = self.advance(true) {
                return done;
            }
            self.park(None);
        }
    }

    /// The producer's source, once `onStart` has named it, as a quiet wait's ping.
    #[cfg(feature = "tokio")]
    fn ping_target(&self) -> Option<Ping> {
        let source = self.stream.lock().source_binder.clone()?;
        Ping::to(&source, self.ping)
    }

    /// Wait for a batch, the end or a grant to send, up to `deadline` or one ping.
    fn park(&self, deadline: Option<Instant>) {
        let source = self.stream.lock().source_binder.clone();
        let ping = source
            .as_ref()
            .and_then(|source| Ping::to(source, self.ping));
        // With no source yet there is nobody to ping, so its arrival ends the wait too.
        let starting = source.is_none() && self.ping != PingPolicy::Off;
        // `must_wait` is read under the lock, so a batch landing after `advance` looked wakes it.
        park(
            &self.stream.state,
            &self.stream.arrived,
            |s: &StreamState| {
                let blocked = s.must_wait() && !(starting && s.source_binder.is_some());
                #[cfg(test)]
                if let Some(parked) = s.parked.as_ref().filter(|_| blocked) {
                    let _ = parked.send(());
                }
                blocked
            },
            deadline,
            ping.as_ref(),
        );
    }

    pub(super) fn try_recv(&mut self) -> BinderResult<Option<T>> {
        self.retry_armed = true;
        match self.advance(false) {
            Some(done) => done.transpose(),
            None => Ok(None),
        }
    }

    pub(super) fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        // A `timeout` past `Instant`'s range waits unbounded, not panics, so it acts as a `recv`.
        let deadline = Instant::now().checked_add(timeout);
        self.retry_armed = true;
        loop {
            if let Some(done) = self.advance(deadline.is_none()) {
                return done.transpose();
            }
            // Checked here too, so `timeout` bounds the call whatever sends it round the loop.
            if deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(None);
            }
            self.park(deadline);
        }
    }

    #[cfg(feature = "tokio")]
    pub(super) async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        self.retry_armed = true;
        loop {
            // Created before looking, so a batch landing before the await is not missed.
            let stream = self.stream.clone();
            let notified = stream.notify.notified();
            if let Some(done) = self.advance_async().await {
                return done;
            }
            let Some(ping) = self.ping_target() else {
                notified.await;
                continue;
            };
            let abandon = Abandon::new(&stream.state, &stream.arrived);
            let gone = abandon.gone();
            // No `tokio/time` to suspend against: the wait, and the ping ending it, use the pool.
            let parked = on_pool(self.stream.clone(), move |stream| {
                park(
                    &stream.state,
                    &stream.arrived,
                    |s| s.must_wait() && !gone.load(Ordering::Acquire),
                    None,
                    Some(&ping),
                );
                Ok(())
            })
            .await;
            drop(abandon);
            if parked.is_err() {
                // No pool: wait unpinged rather than spin.
                notified.await;
            }
        }
    }

    /// An item, the next batch decoded, or the end; `None` (nothing queued) grants owed credit.
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
                    // A failed grant may have ended the stream; look again, don't wait.
                    let ended = self.stream.lock().end.is_some();
                    if !ended {
                        return None;
                    }
                }
            }
        }
    }

    /// [`advance`](Self::advance) with the grants moved off the executor thread.
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
                            // Still owed if unrun; the idle grant below settles it.
                            let _ = on_pool(grant, send_grant).await;
                        }
                    }
                    Err(e) => {
                        let (answer, cancel) = self.batch_failed(e);
                        // Not awaited: a drop at an await would lose the error this call yields.
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
                        // No pool: the grant never goes out, and nothing wakes a parked consumer.
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

    /// An item already decoded, or the end of a stream whose outcome was reported.
    fn ready(&mut self) -> Option<Option<BinderResult<T>>> {
        if let Some(item) = self.decoded.pop_front() {
            return Some(Some(Ok(item)));
        }
        self.finished.then_some(None)
    }

    /// Report an undecodable batch and cancel: no grant follows it, so the producer would park.
    fn batch_failed(&mut self, e: StatusCode) -> (Option<BinderResult<T>>, CancelDue) {
        self.finished = true;
        // Overriding: the consumer is handed this error now, even if the producer's end is queued.
        let cancel = self.stream.fail(Status::from(e), true);
        (Some(Err(e.into())), cancel)
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
                // Items from a malformed batch would contradict the error, so none are handed out.
                self.decoded.clear();
                Err(e)
            }
        }
    }

    fn decode_batch(&mut self, bytes: &[u8], count: i32) -> Result<()> {
        let mut parcel = Parcel::from_slice(bytes);
        for decoded in 0..count {
            // `count` is off the wire: unchecked, an empty batch claiming `i32::MAX` items spins.
            if parcel.data_avail() == 0 {
                log::error!("stream: batch claims {count} items, bytes ran out after {decoded}");
                return Err(StatusCode::NotEnoughData);
            }
            self.decoded.push_back(parcel.read::<T>()?);
        }
        if parcel.data_avail() != 0 {
            // More bytes than `count` items decode to; reading on would guess where an item starts.
            log::error!(
                "stream: {} bytes left after {count} items",
                parcel.data_avail()
            );
            return Err(StatusCode::BadValue);
        }
        Ok(())
    }

    /// The grant due now; on a starved producer's last chance, a retry that fails ends the stream.
    fn grant_due(&mut self, all: bool, blocking: bool) -> Option<GrantToken> {
        let mut state = self.stream.lock();
        if state.owed() == 0 {
            return None;
        }
        let last_chance = blocking && state.producer_may_be_starved();
        if all {
            // One retry per call: the failure likely persists; retrying at once never returns.
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

    /// End reached and every earlier item handed out: report it once, then read as finished.
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
        self.finished
            .then(|| self.stream.lock().end.clone())
            .flatten()
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
            // Dropping the recipient only makes the link inert; the source's list never shrinks.
            unlink_death(&binder, &death);
        }
        if let Some(source) = source {
            // Nothing else tells a producer parked on `wait_credit` that nobody reads any more.
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

    /// Consumer and producer over a local sink binder; the wire is `tests/stream_rpc.rs`.
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
        /// Refuse as a transport that cannot say whether the call arrived, not as a session does.
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

    /// A consumer whose hand-driven source has sent `onStart` with `credits`.
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

    /// A departed item is delivered or lost; only sure non-arrival refunds, else the stream breaks.
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

    /// Refused, dead, timed out or `WouldBlock` left nothing to dispatch; other failures may have.
    #[test]
    fn what_a_failed_send_says_about_the_batch() {
        for (e, known) in [
            (StatusCode::FailedTransaction, true),
            (StatusCode::DeadObject, true),
            (StatusCode::TimedOut, true),
            (StatusCode::WouldBlock, true),
            (StatusCode::Unknown, false),
            (StatusCode::BadValue, false),
        ] {
            assert_eq!(certainly_not_delivered(e), known, "{e:?}");
        }
    }

    /// Unknown delivery ends the producer (credit left is unknown); the terminator spends none.
    #[test]
    fn a_send_that_cannot_say_whether_it_arrived_ends_the_producer() {
        let recorded = Arc::new(Recorded::default());
        recorded.unknown.store(true, Ordering::SeqCst);
        recorded.refuse.store(1, Ordering::SeqCst);
        let sink_binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        // Eight bytes to a batch: every second `i32` is what sends one.
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(8, 4)).expect("open");

        sink.send(&0, None).expect("queued");
        assert_eq!(sink.send(&1, None).err(), Some(StatusCode::Unknown));
        assert_eq!(
            sink.send(&2, None).err(),
            Some(StatusCode::Unknown),
            "every later send reports it, one that would only have queued too"
        );
        assert_eq!(
            sink.pending(),
            0,
            "and queues nothing, since nothing will send it"
        );
        assert_eq!(sink.flush(None).err(), Some(StatusCode::Unknown));
        assert_eq!(
            sink.credits(),
            3,
            "the credit is neither spent twice nor returned"
        );
        assert_eq!(
            sink.terminate(None, ExceptionCode::None as i32, 0, None)
                .err(),
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

    /// A grant's total counts once however it goes; a refusal nothing will retry ends the stream.
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
                assert_eq!(rx.stream.lock().end.is_some(), gave_up, "{case}");
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

    /// Producer side: only the highest total seen counts, so a repeated grant adds nothing.
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

    /// An unrun grant holds nothing back: its batches stay owed and the next call announces them.
    #[test]
    fn a_grant_nobody_sent_is_sent_by_the_next_call() {
        let (mut rx, _sink, calls) = started::<i32>(2, 4, 4);
        rx.stream.lock().drained = 1;
        drop(rx.grant_due(false, false).expect("one batch is owed"));
        assert_eq!(calls.attempts.load(Ordering::SeqCst), 0);

        assert_eq!(rx.try_recv().expect("no error"), None);
        assert_eq!(calls.granted.load(Ordering::SeqCst), 1);
    }

    /// Exactly one terminator on every way out; a lost one blocks a consumer no death link frees.
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

    /// A pool dropping the task unrun: the end still goes out from `Drop`, and that is reported.
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
            handle.block_on(sink.terminate_async(None, ExceptionCode::None as i32, 0, None)),
            Ok(())
        );
        assert_eq!(recorded.ends.load(Ordering::SeqCst), 1);
    }

    /// No pool for the terminator: `ended` is already set, so the token's `Drop` must send it.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_the_pool_cannot_take_still_terminates_the_stream() {
        let recorded = Arc::new(Recorded::default());
        let binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        let sink = Producer::<i32>::open(&binder, &SinkPolicy::default()).expect("a local sink");

        // No runtime here, so `spawn_blocking` panics with the call in hand.
        let polled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut end =
                std::pin::pin!(sink.terminate_async(None, ExceptionCode::None as i32, 0, None));
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

    /// A consumer with no pool must end the stream: parking would wait on a starved producer.
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
            // Sent back alive: dropping the consumer also cancels and would race the count below.
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
            sink.send(item, None).expect("send");
        }
        sink.terminate(None, ExceptionCode::None as i32, 0, None)
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

    /// Plan 10-7 AC-7.2: 4-byte batches, one opening credit; item 2 waits for the first drain.
    #[test]
    fn a_full_window_stops_the_producer_until_the_consumer_drains() {
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let (progress, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            for item in 0..4i32 {
                if sink.send(&item, None).is_err() {
                    return;
                }
                let _ = progress.send(item);
            }
            let _ = sink.terminate(None, ExceptionCode::None as i32, 0, None);
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

    /// In flight is the window, not twice it: one drained batch buys exactly one more.
    #[test]
    fn draining_a_batch_buys_exactly_one_batch_more() {
        // Window 2 puts the grant threshold at one batch; four-byte batches make a batch one `i32`.
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(2, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let producer = thread::spawn(move || {
            for item in 0..1000i32 {
                if sink.send(&item, None).is_err() {
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

    /// One opening batch, window eight, runs to the end: clamp or idle grant pays each batch back.
    #[test]
    fn an_opening_window_below_the_grant_threshold_does_not_stall() {
        // Half of eight would be four; held to the one batch the producer opened with.
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(8, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let mut sent = Ok(());
            for item in 0..20i32 {
                sent = sink.send(&item, None);
                if sent.is_err() {
                    break;
                }
            }
            let _ = done.send(
                sent.and_then(|()| sink.terminate(None, ExceptionCode::None as i32, 0, None)),
            );
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

    /// A consumer dropped before `onStart` answers it with a cancel on arrival.
    #[test]
    fn a_consumer_dropped_before_the_stream_starts_cancels_it_on_arrival() {
        let (rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        drop(rx);

        let mut sink = Producer::<i32>::open(&sink_binder, &SinkPolicy::default())
            .expect("the sink object is still there");
        assert!(sink.is_canceled(), "`onStart` must have been answered");
        // Refused on entry, as on the ring, not queued into a batch that will never leave.
        assert_eq!(
            sink.send(&1, None).err(),
            Some(StatusCode::InvalidOperation)
        );
        assert_eq!(sink.pending(), 0);
    }

    /// An async producer awaiting credit yields the thread; on one thread the consumer needs it.
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
                // `tokio::spawn` also checks that the futures are `Send`.
                let producer = tokio::spawn(async move {
                    // Spends the only credit.
                    sink.send_async(&0, None).await?;
                    spent.notify_one();
                    // So this one finds none, and nothing is reading yet.
                    for item in 1..50i32 {
                        sink.send_async(&item, None).await?;
                    }
                    sink.terminate_async(None, ExceptionCode::None as i32, 0, None)
                        .await
                });
                // Held back until the producer is out of credit, so its next send has to wait.
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

    /// A bounded async credit wait ends at its deadline and takes the item back out of the batch.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_send_async_out_of_credit_times_out_and_the_item_is_not_queued() {
        let timeout = Duration::from_millis(50);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");
        runtime
            .block_on(sink.send_async(&0, Some(timeout)))
            .expect("the opening credit");

        let (done, watch) = mpsc::channel();
        let handle = runtime.handle().clone();
        thread::spawn(move || {
            let before = Instant::now();
            let sent = handle.block_on(sink.send_async(&1, Some(timeout)));
            let _ = done.send((sent, before.elapsed(), sink));
        });
        let (sent, elapsed, mut sink) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the timeout must end the wait for credit");
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert_eq!(sink.pending(), 0, "the item was taken back out");

        // Window 1: draining the batch grants its credit back.
        assert_eq!(rx.recv().expect("item 0").expect("ok"), 0);
        runtime
            .block_on(sink.send_async(&1, Some(timeout)))
            .expect("a grant came");
        runtime
            .block_on(sink.terminate_async(None, ExceptionCode::None as i32, 0, None))
            .expect("end");
        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        assert!(rx.recv().is_none());
        assert!(rx.end_status().expect("ended").is_ok());
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    #[test]
    fn a_cancel_releases_a_producer_waiting_for_credit() {
        let (rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");

        let (outcome, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            // The first send spends the opening credit; the second parks.
            let first = sink.send(&0, None);
            let second = sink.send(&1, None);
            let _ = outcome.send((first, second, sink.is_canceled()));
        });

        // Nothing drains, so the producer is parked; only cancel can release it.
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

    /// A dropped producer still flushes and ends the stream; nothing else frees the consumer.
    #[test]
    fn a_dropped_producer_flushes_and_reports_that_it_never_ended() {
        let (mut sink, mut rx) = default_pair::<i32>();
        sink.send(&1, None).expect("send");
        sink.send(&2, None).expect("send");
        assert_eq!(sink.pending(), 2, "neither item filled a 16 KB batch");
        drop(sink);

        assert_eq!(rx.recv().expect("item 1").expect("ok"), 1);
        assert_eq!(rx.recv().expect("item 2").expect("ok"), 2);
        let failure = rx.recv().expect("a terminator").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(rx.recv().is_none(), "the stream is over");
    }

    /// Same drop with no credit: the item is lost, as waiting would park the dropping thread.
    #[test]
    fn a_dropped_producer_with_no_credit_still_ends_the_stream() {
        let (mut rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        // Eight bytes to a batch is two `i32`s, and one opening credit.
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(8, 1)).expect("open");

        sink.send(&0, None).expect("send");
        // Fills the batch, so this one spends the only credit.
        sink.send(&1, None).expect("send");
        // Queued with no credit left and nothing draining, so no grant is coming either.
        sink.send(&2, None).expect("send");
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

    /// Earlier losses reach the terminator: `EX_NONE` over lost items reads as a clean end.
    #[test]
    fn items_a_failed_batch_lost_are_reported_by_the_terminator() {
        let recorded = Arc::new(Recorded::default());
        recorded.refuse.store(1, Ordering::SeqCst);
        let sink_binder = BnStreamSink::new_binder(RefusingSink(recorded.clone())).as_binder();
        // One `i32` per batch, and enough credit that nothing here waits.
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 4)).expect("open");

        sink.send(&0, None).expect_err("the first batch is refused");
        // The failure cost that item only: the stream is still usable, as `send`'s rustdoc says.
        sink.send(&1, None).expect("send");
        sink.terminate(None, ExceptionCode::None as i32, 0, None)
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

    /// A count its bytes don't match is refused; items decoded before the mismatch are withheld.
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

    /// `oneway` drops `onBatch`'s error, so the stream must end or a skipped batch goes unseen.
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

    /// Credit bounds the queue: a producer gets its opening window plus what was granted.
    #[test]
    fn a_batch_sent_without_credit_ends_the_stream() {
        // Window 2 with one declared puts the threshold at one, so the drain below pays at once.
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
        // Polled: without the refusal a wait would hang instead of failing.
        assert_eq!(rx.try_recv().expect("the batch before it"), Some(2));
        assert_eq!(
            rx.try_recv().expect_err("an error").exception_code(),
            ExceptionCode::IllegalState
        );
    }

    /// Same-object `oneway` calls stay ordered, so a batch before `onStart` has no credit at all.
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

    /// The consumer caps the opening window and refuses at the start, not part-way through.
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
            rx.stream.lock().end.is_none(),
            "a consumer set up for it takes it"
        );
        assert_eq!(calls.canceled.load(Ordering::SeqCst), 0);

        // The producer's side of the same refusal.
        let (_rx, sink_binder) = Consumer::<i32>::new(&ReceiverPolicy::default());
        let sink = Producer::<i32>::open(&sink_binder, &sink_policy(16, 5))
            .expect("`onStart` is oneway, so the refusal is not an error here");
        assert!(sink.is_canceled());
    }

    /// A second `onStart`, whatever it states, must not end the running stream.
    #[test]
    fn a_second_on_start_cannot_end_the_stream() {
        let (rx, sink, _calls) = started::<i32>(4, 4, 4);
        let other = Arc::new(SourceCalls::default());
        let intruder = BnStreamSource::new_binder(RefusingSource(other.clone())).as_binder();

        sink.r#onStart(&intruder, i32::MAX).expect("onStart");

        assert!(rx.stream.lock().end.is_none());
        assert_eq!(other.canceled.load(Ordering::SeqCst), 1);

        // Nor one whose source is not a source at all.
        let (_other_rx, not_a_source) = Consumer::<i32>::new(&ReceiverPolicy::default());
        sink.r#onStart(&not_a_source, 4).expect("onStart");
        assert!(rx.stream.lock().end.is_none());

        // Nor one that passed the early check before the first was kept.
        let refused = Status::from(ExceptionCode::IllegalArgument);
        assert!(
            rx.stream.refuse_start(&intruder, refused),
            "another producer is cancelled"
        );
        assert!(rx.stream.lock().end.is_none());
    }

    /// The grant threshold, half a window, is capped at the opening window a producer can fill.
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

    /// Every grant failing, producer alive, no death link: retries must not raise what it may send.
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

        // Four announced whether or not any arrived: four more batches is the producer's ceiling.
        for item in 4..8u8 {
            sink.r#onBatch(&[item, 0, 0, 0], 1)
                .expect("granted, perhaps");
        }
        assert!(
            sink.r#onBatch(&[8, 0, 0, 0], 1).is_err(),
            "a hundred retries are not a hundred grants"
        );
    }

    /// A failed grant is retried by the next poll: not in-call (it would loop), not fatal.
    #[test]
    fn a_grant_that_fails_is_retried_by_the_next_poll() {
        let (mut rx, sink, calls) = started::<i32>(2, 4, 1);
        calls.refuse.store(2, Ordering::SeqCst);
        sink.r#onBatch(&[1, 0, 0, 0], 1).expect("onBatch");

        // The producer has used all it is known to have: a blocking consumer would give up here.
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

    /// Nobody re-calls a blocking recv: a failed grant ends it only if the producer may starve.
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

    /// A wait begun before `onStart` ends when it lands, so the caller can look up whom to ping.
    #[test]
    fn a_wait_begun_before_the_producer_starts_ends_when_it_starts() {
        let (rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(4, 4));
        let (parked_tx, parked) = mpsc::channel();
        rx.stream.lock().parked = Some(parked_tx);
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            rx.park(None);
            // Sent back alive: dropping the consumer cancels too.
            let _ = done.send(rx);
        });
        // Signalled under the lock the wait only gives up by sleeping, so `onStart` comes after.
        parked
            .recv_timeout(Duration::from_secs(5))
            .expect("the wait never slept");
        let source = BnStreamSource::new_binder(RefusingSource(Arc::default())).as_binder();
        let sink: Strong<dyn IStreamSink> =
            FromIBinder::try_from(sink_binder).expect("the consumer's own sink");
        sink.r#onStart(&source, 4).expect("onStart");
        let _rx = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the producer's start must end a wait that had nobody to ping");
    }

    /// A dropped `send_async` ends its pooled credit wait, freeing the pool's only thread.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_dropped_send_async_gives_back_the_thread_its_credit_wait_held() {
        let (_rx, sink_binder) = Consumer::<i32>::new(&receiver_policy(1, 4));
        let mut sink = Producer::<i32>::open(&sink_binder, &sink_policy(4, 1)).expect("open");
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .max_blocking_threads(1)
                .build()
                .expect("runtime");
            let freed = runtime.block_on(async {
                sink.send_async(&0, None).await.expect("the opening credit");
                {
                    // A deadline sends the wait to the pool; the first poll hands it over.
                    let send = sink.send_async(&1, Some(Duration::from_secs(60)));
                    let mut send = std::pin::pin!(send);
                    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                    assert!(std::future::Future::poll(send.as_mut(), &mut cx).is_pending());
                }
                tokio::task::spawn_blocking(|| ()).await
            });
            let _ = done.send(freed.is_ok());
        });
        assert!(watch
            .recv_timeout(Duration::from_secs(10))
            .expect("the abandoned credit wait still holds the pool's only thread"));
    }
}

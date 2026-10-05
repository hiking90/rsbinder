// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The kernel path: records on a Fast Message Queue ring.
//!
//! The consumer makes the ring (`rsbinder-fmq`, one memfd, sealed and
//! allocated up front) and describes it in the endpoint; the producer
//! attaches. From then on no binder call carries an item: the producer
//! writes one record per item and waits on the ring's EventFlag when
//! the ring is full, the consumer reads records and waits when it is
//! empty. The two binders in play are only watched for death. See
//! `StreamEndpoint.aidl` for the record layout, the reserve for the end
//! record and the EventFlag bits, which a C++ peer implements against.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};
use std::time::{Duration, Instant};

use rsbinder_fmq::{AttachPolicy, Descriptor, EventFlag, MessageQueue, NOT_EMPTY, NOT_FULL};

use crate::binder::Interface;
use crate::error::{Result, StatusCode};
use crate::fmq::{MQDescriptor, SynchronizedReadWrite};
use crate::parcel::Parcel;
use crate::parcelable::{Deserialize, Serialize};
use crate::status::{BinderResult, ExceptionCode, Status};
use crate::SIBinder;

use super::generated::rsbinder::stream::IStreamSink::{BnStreamSink, IStreamSink};
#[cfg(feature = "tokio")]
use super::pool::on_pool;
use super::{
    decode_item, dropped_terminator, encode_item, status_from_fields, truncated_terminator,
    unlink_death, watch_death, ReceiverPolicy, SinkPolicy, END_RESERVE,
};

/// The descriptor type the endpoint carries: a ring of bytes.
pub(super) type Ring = MQDescriptor<i8, SynchronizedReadWrite>;

/// EventFlag bit the consumer sets to want no more; only the producer waits on it and consumes it.
pub(super) const CANCEL: u32 = 0x04;

/// Bytes of record header: a little-endian `u32`.
const HEADER: usize = 4;
/// The header's kind bit; the other 31 bits are the payload length.
const KIND_END: u32 = 1 << 31;
const LEN_MASK: u32 = !KIND_END;
/// The two `i32` fields at the front of an end record's payload.
const END_FIELDS: usize = 8;
/// Bytes of message an end record can carry within the reserve.
const END_MESSAGE_MAX: usize = END_RESERVE - HEADER - END_FIELDS;

/// How long a side looks at the ring before it parks; a park costs a wake syscall on each side.
const SPIN: Duration = Duration::from_micros(20);
/// Looks between clock reads while spinning.
const SPIN_CLOCK_EVERY: u32 = 32;
/// Committed bytes a refill waits for while the producer is still adding: reading right behind
/// it takes the cache line it is writing, once per record.
const MIN_BATCH: usize = 4096;

/// The spin for a wait ending at `deadline`; none on one core, where spinning holds up the peer.
fn spin_budget(deadline: Option<Instant>) -> Duration {
    static MULTICORE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let multicore =
        *MULTICORE.get_or_init(|| std::thread::available_parallelism().is_ok_and(|n| n.get() > 1));
    match deadline {
        _ if !multicore => Duration::ZERO,
        Some(deadline) => SPIN.min(deadline.saturating_duration_since(Instant::now())),
        None => SPIN,
    }
}

// --- The ring, as both ends and the pool see it ---

/// One end's ring handle, shared with its pool task and the death recipient that wakes it.
struct Shared {
    /// This end is the queue's one reader or writer; the lock lets a pool task and owner alternate.
    queue: Mutex<MessageQueue<u8>>,
    flag: EventFlag,
    /// Ring bytes.
    capacity: usize,
    /// The peer's process is gone.
    dead: AtomicBool,
    /// Producer: `CANCEL` was seen; latched, since the wait that sees the bit consumes it.
    canceled: AtomicBool,
    /// Producer: `Drop` gave up on the record in transit.
    abandoned: AtomicBool,
    /// Consumer: how the stream ended, once it has.
    end: Mutex<Option<Status>>,
}

impl Shared {
    fn new(queue: MessageQueue<u8>, flag: EventFlag, capacity: usize) -> Self {
        Shared {
            queue: Mutex::new(queue),
            flag,
            capacity,
            dead: AtomicBool::new(false),
            canceled: AtomicBool::new(false),
            abandoned: AtomicBool::new(false),
            end: Mutex::new(None),
        }
    }

    fn queue(&self) -> std::sync::MutexGuard<'_, MessageQueue<u8>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn end(&self) -> Option<Status> {
        self.end.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Record how the stream ended; `overriding` replaces it, otherwise the first word stands.
    fn set_end(&self, status: Status, overriding: bool) {
        let mut end = self.end.lock().unwrap_or_else(|e| e.into_inner());
        if overriding || end.is_none() {
            *end = Some(status);
        }
    }

    /// Write one record: no room is `Ok(false)` for `Wait::Never`; only an item yields to `CANCEL`.
    fn write_record(
        &self,
        header: u32,
        payload: &[u8],
        wait: Wait,
    ) -> std::result::Result<bool, WriteFailure> {
        let is_end = header & KIND_END != 0;
        let limit = if is_end {
            self.capacity
        } else {
            self.capacity - END_RESERVE
        };
        let n = HEADER + payload.len();
        loop {
            if self.dead.load(Ordering::SeqCst) {
                return Err(WriteFailure::Dead);
            }
            if !is_end && (self.canceled.load(Ordering::SeqCst) || self.flag.peek() & CANCEL != 0) {
                self.canceled.store(true, Ordering::SeqCst);
                return Err(WriteFailure::Canceled);
            }
            // `Drop` sets this, then writes the end record itself, so only an item gives up.
            if !is_end && self.abandoned.load(Ordering::SeqCst) {
                return Err(WriteFailure::Abandoned);
            }
            {
                let mut queue = self.queue();
                // The only writer: the reader's counter is reloaded only when it shows no room.
                let reserved = match queue
                    .begin_write_cached(n, limit)
                    .map_err(WriteFailure::broken)?
                {
                    Some(mut regions) => {
                        regions
                            .write_at(0, &header.to_le_bytes())
                            .map_err(WriteFailure::broken)?;
                        regions
                            .write_at(HEADER, payload)
                            .map_err(WriteFailure::broken)?;
                        true
                    }
                    None => false,
                };
                if reserved {
                    queue.commit_write_cached(n).map_err(WriteFailure::broken)?;
                    drop(queue);
                    // Committed is delivered: libfmq's `writeBlocking` ignores a failed wake too.
                    if let Err(e) = self.flag.wake(NOT_EMPTY) {
                        log::error!("stream: the wake after a committed record failed: {e:?}");
                    }
                    return Ok(true);
                }
            }
            let timeout = match wait {
                Wait::Never => return Ok(false),
                Wait::Forever => None,
                Wait::Until(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return Err(WriteFailure::TimedOut);
                    }
                    Some(left)
                }
            };
            let seen = match self.flag.wait(NOT_FULL | CANCEL, timeout) {
                Ok(seen) => seen,
                Err(rsbinder_fmq::Error::TimedOut) => return Err(WriteFailure::TimedOut),
                Err(e) => return Err(WriteFailure::broken(e)),
            };
            if seen & CANCEL != 0 {
                self.canceled.store(true, Ordering::SeqCst);
                if !is_end {
                    return Err(WriteFailure::Canceled);
                }
            }
        }
    }
}

/// Why a record could not be written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WriteFailure {
    /// The consumer's process is gone.
    Dead,
    /// The consumer set `CANCEL`.
    Canceled,
    /// The producer's `Drop` gave the record up.
    Abandoned,
    /// `SinkPolicy::send_timeout` expired with no room; the ring is untouched and still usable.
    TimedOut,
    /// The counters fail their invariant or the futex failed; nothing more can be written.
    Broken(StatusCode),
}

impl WriteFailure {
    fn broken(e: rsbinder_fmq::Error) -> Self {
        WriteFailure::Broken(StatusCode::from(e))
    }

    fn code(self) -> StatusCode {
        match self {
            WriteFailure::Dead => StatusCode::DeadObject,
            WriteFailure::Canceled => StatusCode::InvalidOperation,
            // Never handed to the ring, so it did not arrive.
            WriteFailure::Abandoned => StatusCode::FailedTransaction,
            WriteFailure::TimedOut => StatusCode::TimedOut,
            WriteFailure::Broken(e) => e,
        }
    }
}

/// What a pool task left behind: one at most, which the owner waits for before writing or waiting.
#[derive(Default)]
struct TransitState {
    /// Items accepted and never written. Only the terminator reports them.
    lost: i32,
    /// A failure with nobody told yet; the next call returns it.
    unreported: Option<StatusCode>,
    /// The ring is unusable; every later call returns this.
    broken: Option<StatusCode>,
}

#[derive(Default)]
struct Transit {
    state: Mutex<TransitState>,
    /// Written under `state`'s lock; read without it on the fast path.
    in_transit: std::sync::atomic::AtomicBool,
    /// `unreported` or `broken` was ever set; never cleared, so while it is down neither is.
    flagged: std::sync::atomic::AtomicBool,
    idle: Condvar,
    #[cfg(feature = "tokio")]
    notify: tokio::sync::Notify,
}

impl Transit {
    fn lock(&self) -> std::sync::MutexGuard<'_, TransitState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[cfg(feature = "tokio")]
    fn begin(&self) {
        let _guard = self.lock();
        self.in_transit.store(true, Ordering::SeqCst);
    }

    fn in_transit(&self) -> bool {
        self.in_transit.load(Ordering::SeqCst)
    }

    fn lost(&self) -> i32 {
        self.lock().lost
    }

    fn take_unreported(&self) -> Option<StatusCode> {
        self.lock().unreported.take()
    }

    fn broken(&self) -> Option<StatusCode> {
        self.lock().broken
    }

    fn mark_broken(&self, e: StatusCode) {
        self.lock().broken.get_or_insert(e);
        self.flagged.store(true, Ordering::SeqCst);
    }

    /// Nothing to wait out and nothing ever left to report: the owner may skip the lock.
    fn quiet(&self) -> bool {
        // `settle` raises `flagged` before it lowers `in_transit`, so this order sees both.
        !self.in_transit.load(Ordering::SeqCst) && !self.flagged.load(Ordering::SeqCst)
    }

    /// The token is done: leave `failure` for the next call; `lost` = its item did not go in.
    #[cfg(feature = "tokio")]
    fn settle(&self, failure: Option<StatusCode>, lost: bool, broken: bool) {
        {
            let mut state = self.lock();
            if lost {
                state.lost = state.lost.saturating_add(1);
            }
            if let Some(e) = failure {
                state.unreported = Some(e);
                if broken {
                    state.broken.get_or_insert(e);
                }
                self.flagged.store(true, Ordering::SeqCst);
            }
            self.in_transit.store(false, Ordering::SeqCst);
        }
        self.idle.notify_all();
        #[cfg(feature = "tokio")]
        self.notify.notify_waiters();
    }

    /// The awaiting future takes back its own record's timeout: reported to it, so not lost.
    #[cfg(feature = "tokio")]
    fn reclaim_timed_out(&self) -> bool {
        let mut state = self.lock();
        if state.unreported != Some(StatusCode::TimedOut) {
            return false;
        }
        state.unreported = None;
        state.lost = state.lost.saturating_sub(1);
        true
    }

    /// Block until nothing is in transit; `false` if `deadline` passes first.
    fn wait_idle_until(&self, deadline: Option<Instant>) -> bool {
        let mut state = self.lock();
        while self.in_transit.load(Ordering::SeqCst) {
            let Some(deadline) = deadline else {
                state = self.idle.wait(state).unwrap_or_else(|e| e.into_inner());
                continue;
            };
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            state = self
                .idle
                .wait_timeout(state, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        true
    }

    fn wait_idle(&self) {
        self.wait_idle_until(None);
    }

    #[cfg(feature = "tokio")]
    async fn idle_async(&self) {
        loop {
            // Made before the check: a `Notified` gets `notify_waiters` from its creation on.
            let notified = self.notify.notified();
            if !self.in_transit.load(Ordering::SeqCst) {
                return;
            }
            notified.await;
        }
    }
}

// --- Records ---

fn item_header(len: usize) -> u32 {
    // Checked against the ring before any header is built.
    len as u32
}

fn end_header(len: usize) -> u32 {
    KIND_END | len as u32
}

/// The longest prefix of `message` within `max` bytes that ends on a character boundary.
fn fitted(message: &str, max: usize) -> &str {
    if message.len() <= max {
        return message;
    }
    let mut end = max;
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    &message[..end]
}

/// The end record's payload: the three `onEnd` arguments, the message cut to fit the reserve.
fn end_payload(exception: i32, service_specific: i32, message: Option<&str>) -> Vec<u8> {
    let mut payload = Vec::with_capacity(END_RESERVE - HEADER);
    payload.extend_from_slice(&exception.to_le_bytes());
    payload.extend_from_slice(&service_specific.to_le_bytes());
    if let Some(message) = message {
        payload.extend_from_slice(fitted(message, END_MESSAGE_MAX).as_bytes());
    }
    payload
}

/// The status an end record carries; the reader checked `payload` holds `END_FIELDS`.
fn end_status(payload: &[u8]) -> Status {
    let exception = i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
    let service_specific = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let message = (payload.len() > END_FIELDS)
        .then(|| String::from_utf8_lossy(&payload[END_FIELDS..]).into_owned());
    match status_from_fields(exception, service_specific, message.as_deref()) {
        Ok(status) => status,
        // Unreadable, yet the stream must stop; the decode failure is why.
        Err(code) => Status::from(code),
    }
}

// --- Producer ---

/// Sink death: a producer parked on a full ring has no call in flight that would fail.
struct ProducerDeath(Arc<Shared>);

impl crate::DeathRecipient for ProducerDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        self.0.dead.store(true, Ordering::SeqCst);
        // The producer's own mask: wakes the waiter on this side, which then finds `dead`.
        let _ = self.0.flag.wake(NOT_FULL);
    }
}

/// A record the pool carries to the ring; every exit, unwind included, settles in `Drop`.
#[cfg(feature = "tokio")]
struct RecordInTransit {
    shared: Arc<Shared>,
    transit: Arc<Transit>,
    bytes: Vec<u8>,
    /// The `send_async` call's deadline, kept by the pool wait even if the future is dropped.
    wait: Wait,
    outcome: std::result::Result<(), WriteFailure>,
}

#[cfg(feature = "tokio")]
impl RecordInTransit {
    fn new(shared: Arc<Shared>, transit: Arc<Transit>, bytes: Vec<u8>, wait: Wait) -> Self {
        transit.begin();
        RecordInTransit {
            shared,
            transit,
            bytes,
            wait,
            // What a pool task that never runs leaves it as.
            outcome: Err(WriteFailure::Abandoned),
        }
    }

    fn send(mut self) {
        // What an unwind out of the write leaves it as.
        self.outcome = Err(WriteFailure::Broken(StatusCode::Unknown));
        self.outcome =
            match self
                .shared
                .write_record(item_header(self.bytes.len()), &self.bytes, self.wait)
            {
                Ok(true) => Ok(()),
                // Never `Wait::Never`: it returns only with the record in or a failure.
                Ok(false) => Err(WriteFailure::Broken(StatusCode::BadValue)),
                Err(failure) => Err(failure),
            };
    }
}

#[cfg(feature = "tokio")]
impl Drop for RecordInTransit {
    fn drop(&mut self) {
        match self.outcome {
            Ok(()) => self.transit.settle(None, false, false),
            Err(failure) => self.transit.settle(
                Some(failure.code()),
                true,
                matches!(failure, WriteFailure::Broken(_)),
            ),
        }
    }
}

/// Counts a `send_async` item dropped while an earlier record held the pool: unwritten, unqueued.
#[cfg(feature = "tokio")]
struct Unhanded<'a>(&'a Transit);

#[cfg(feature = "tokio")]
impl Drop for Unhanded<'_> {
    fn drop(&mut self) {
        let mut state = self.0.lock();
        state.lost = state.lost.saturating_add(1);
    }
}

/// The producer over the ring: one record per item, waiting on `NOT_FULL` when full.
pub(super) struct Producer<T: ?Sized> {
    shared: Arc<Shared>,
    transit: Arc<Transit>,
    /// Reused encoding buffer, new after a refusal or a pool hand-off; boxed: `Parcel` is `!Freeze`.
    scratch: Box<Parcel>,
    /// Set once the end record is in, so `Drop` does not write a second.
    ended: bool,
    /// The consumer's sink, kept for the unlink in `Drop`.
    sink: SIBinder,
    /// Holds the death link on the sink; the binder keeps only a `Weak`.
    death: Option<Arc<dyn crate::DeathRecipient>>,
    _item: PhantomData<fn(&T)>,
}

impl<T: Serialize + ?Sized> Producer<T> {
    /// Attach to the consumer's ring and watch its sink for death.
    pub(super) fn open(ring: &Ring, sink: &SIBinder, policy: &SinkPolicy) -> Result<Self> {
        let desc = Descriptor::try_from(ring)?;
        let attach = AttachPolicy {
            max_capacity: policy.max_ring_bytes,
            require_seal: true,
            require_event_flag: true,
        };
        let queue = MessageQueue::<u8>::attach(&desc, &attach).map_err(|e| {
            log::error!("Sink::open: the endpoint's ring cannot be attached: {e}");
            StatusCode::from(e)
        })?;
        let capacity = queue.capacity();
        // An item record needs its header and a payload byte past the reserve.
        if capacity <= END_RESERVE + HEADER {
            log::error!(
                "Sink::open: a ring of {capacity} bytes leaves no room for an item past the \
                 {END_RESERVE}-byte reserve for the end record"
            );
            return Err(StatusCode::BadValue);
        }
        // `require_event_flag` above makes this `Some`.
        let flag = queue.event_flag().ok_or(StatusCode::InvalidOperation)?;
        let shared = Arc::new(Shared::new(queue, flag, capacity));
        let death = watch_death(sink, ProducerDeath(shared.clone()))?;
        Ok(Producer {
            shared,
            transit: Arc::new(Transit::default()),
            scratch: Box::new(Parcel::new_data_only()),
            ended: false,
            sink: sink.clone(),
            death,
            _item: PhantomData,
        })
    }

    /// Encode `item` into `scratch` and check it against the ring. Nothing is written.
    fn encode(&mut self, item: &T) -> Result<()> {
        self.usable()?;
        let encoded = encode_item(&mut self.scratch, item).and_then(|()| {
            let len = self.scratch.ipc_data_size();
            if len > self.max_item_bytes() {
                log::error!(
                    "stream: an item of {len} bytes does not fit a ring of {} bytes, whose \
                     largest item is {} bytes",
                    self.shared.capacity,
                    self.max_item_bytes()
                );
                return Err(StatusCode::BadValue);
            }
            Ok(())
        });
        if encoded.is_err() {
            // Drops a refused item's partial bytes, and the allocation an oversize one grew.
            *self.scratch = Parcel::new_data_only();
        }
        encoded
    }

    /// Write an item record; `Ok(false)` only for `Wait::Never` and a full ring.
    fn write_item(&self, bytes: &[u8], wait: Wait) -> Result<bool> {
        match self
            .shared
            .write_record(item_header(bytes.len()), bytes, wait)
        {
            Ok(written) => Ok(written),
            Err(failure) => {
                if let WriteFailure::Broken(e) = failure {
                    self.transit.mark_broken(e);
                }
                Err(failure.code())
            }
        }
    }

    pub(super) fn send(&mut self, item: &T, deadline: Option<Instant>) -> Result<()> {
        self.encode(item)?;
        // A pool record goes in first, and its unreported failure is returned before any write.
        if !self.transit.quiet() {
            if !self.transit.wait_idle_until(deadline) {
                return Err(StatusCode::TimedOut);
            }
            self.reported()?;
        }
        self.write_item(self.scratch.as_bytes()?, Wait::from_deadline(deadline))
            .map(|_| ())
    }

    /// Nothing is queued here: this waits out a record in transit and reports what it came to.
    pub(super) fn flush(&mut self, deadline: Option<Instant>) -> Result<()> {
        self.usable()?;
        if !self.transit.wait_idle_until(deadline) {
            return Err(StatusCode::TimedOut);
        }
        self.reported()
    }

    pub(super) fn terminate(
        mut self,
        deadline: Option<Instant>,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        // `Drop` runs next and must not write a second end record.
        self.ended = true;
        // The end record must not overtake a record still on the pool.
        let failed = if self.transit.wait_idle_until(deadline) {
            self.transit.take_unreported().or(self.transit.broken())
        } else {
            // Out of time: give the record up as `Drop` does; `finish` reports it lost.
            self.give_up_in_transit();
            let _ = self.transit.take_unreported();
            Some(StatusCode::TimedOut)
        };
        self.finish(failed, exception, service_specific, message)
    }

    /// Encode now; the returned future writes it, from the pool if it has to wait.
    #[cfg(feature = "tokio")]
    pub(super) fn send_async(
        &mut self,
        item: &T,
        timeout: Option<Duration>,
    ) -> impl std::future::Future<Output = Result<()>> + Send + '_ {
        let staged = self.encode(item);
        async move {
            let deadline = super::call_deadline(timeout);
            staged?;
            let unhanded = Unhanded(&self.transit);
            // Needs no deadline of its own: an orphan's deadline is no later than this call's.
            self.transit.idle_async().await;
            std::mem::forget(unhanded);
            self.reported()?;
            // Room now means no pool: the copy is the whole cost.
            if self.write_item(self.scratch.as_bytes()?, Wait::Never)? {
                return Ok(());
            }
            if deadline.is_some_and(|deadline| deadline <= Instant::now()) {
                return Err(StatusCode::TimedOut);
            }
            // The pool task outlives this borrow, so the record takes the bytes with it.
            let bytes =
                std::mem::replace(&mut *self.scratch, Parcel::new_data_only()).into_bytes()?;
            let wait = Wait::from_deadline(deadline);
            let record =
                RecordInTransit::new(self.shared.clone(), self.transit.clone(), bytes, wait);
            // The record settles itself: a dropped future detaches the task, which may never run.
            let _ = on_pool(record, |record| {
                record.send();
                Ok(())
            })
            .await;
            if self.transit.reclaim_timed_out() {
                return Err(StatusCode::TimedOut);
            }
            self.reported()
        }
    }
}

// Writing the end record, and what `Drop` needs, take no `Serialize`.
impl<T: ?Sized> Producer<T> {
    /// The largest item this ring takes.
    fn max_item_bytes(&self) -> usize {
        self.shared.capacity - END_RESERVE - HEADER
    }

    /// Give up the pool's record, woken via this side's own mask; it settles as lost at once.
    fn give_up_in_transit(&self) {
        if self.transit.in_transit() {
            self.shared.abandoned.store(true, Ordering::SeqCst);
            let _ = self.shared.flag.wake(NOT_FULL);
            self.transit.wait_idle();
        }
    }

    /// The error that broke the ring, if one has.
    fn usable(&self) -> Result<()> {
        if !self.transit.flagged.load(Ordering::SeqCst) {
            return Ok(());
        }
        self.transit.broken().map_or(Ok(()), Err)
    }

    /// A transit failure, returned once; a broken ring's error, returned on every call.
    fn reported(&self) -> Result<()> {
        let unreported = self.transit.take_unreported();
        unreported.or(self.transit.broken()).map_or(Ok(()), Err)
    }

    /// Write the end record; `failed` is what an earlier record left unreported.
    fn finish(
        &self,
        failed: Option<StatusCode>,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        if self.shared.dead.load(Ordering::SeqCst) || failed == Some(StatusCode::DeadObject) {
            // Nobody is left to tell.
            return Err(StatusCode::DeadObject);
        }
        let canceled = self.is_canceled();
        // After a cancel no loss is reported: the consumer asked for nothing more.
        let truncated = match truncated_terminator(exception, self.transit.lost()) {
            Some(truncated) if !canceled => Some(truncated),
            _ => None,
        };
        let wrote = match &truncated {
            Some(truncated) => {
                self.write_end(ExceptionCode::IllegalState as i32, 0, Some(truncated))
            }
            None => self.write_end(exception, service_specific, message),
        };
        match failed {
            // The consumer still waits in `recv`, so the end went in; return the earlier failure.
            Some(e) if !canceled => {
                if let Err(end) = wrote {
                    log::warn!("stream: the end record could not be written: {end:?}");
                }
                Err(e)
            }
            _ => wrote,
        }
    }

    /// Never waits: the reserve keeps room, so no room means the consumer moved the read counter.
    fn write_end(
        &self,
        exception: i32,
        service_specific: i32,
        message: Option<&str>,
    ) -> Result<()> {
        let payload = end_payload(exception, service_specific, message);
        match self
            .shared
            .write_record(end_header(payload.len()), &payload, Wait::Never)
        {
            Ok(true) => Ok(()),
            Ok(false) => {
                log::error!(
                    "stream: no room for the end record inside the reserve; the consumer moved \
                     the read counter"
                );
                self.transit.mark_broken(StatusCode::BadValue);
                Err(StatusCode::BadValue)
            }
            Err(failure) => {
                if let WriteFailure::Broken(e) = failure {
                    self.transit.mark_broken(e);
                }
                Err(failure.code())
            }
        }
    }

    #[cfg(feature = "tokio")]
    pub(super) async fn flush_async(&mut self) -> Result<()> {
        self.usable()?;
        self.transit.idle_async().await;
        self.reported()
    }

    /// The end record never waits; the one suspension is for a record a dropped future left.
    #[cfg(feature = "tokio")]
    pub(super) async fn terminate_async(
        mut self,
        exception: i32,
        service_specific: i32,
        message: Option<String>,
    ) -> Result<()> {
        self.transit.idle_async().await;
        let failed = self.transit.take_unreported().or(self.transit.broken());
        // Set only after the wait: a future dropped there must leave `Drop` a stream to end.
        self.ended = true;
        self.finish(failed, exception, service_specific, message.as_deref())
    }

    pub(super) fn is_canceled(&self) -> bool {
        self.shared.canceled.load(Ordering::SeqCst) || self.shared.flag.peek() & CANCEL != 0
    }

    /// An item accepted by `send_async` and not yet in the ring.
    pub(super) fn pending(&self) -> usize {
        usize::from(self.transit.in_transit())
    }
}

impl<T: ?Sized> Drop for Producer<T> {
    /// End the stream without waiting for the consumer; see [`Sink`](super::Sink)'s `Drop`.
    fn drop(&mut self) {
        // Unlink always: a dropped recipient leaves a dead `Weak` in the sink's list for good.
        let death = self.death.take();
        unlink_death(&self.sink, &death);
        if self.ended {
            return;
        }
        // `Drop` does not wait on the consumer.
        self.give_up_in_transit();
        if let Some(e) = self.transit.take_unreported() {
            log::warn!("stream: a record could not be written: {e:?}");
        }
        if self.shared.dead.load(Ordering::SeqCst) {
            return;
        }
        let message = dropped_terminator(self.transit.lost());
        if let Err(e) = self.write_end(ExceptionCode::IllegalState as i32, 0, Some(&message)) {
            log::warn!("stream: the end record could not be written: {e:?}");
        }
    }
}

// --- Consumer ---

/// Producer death ends the stream as `DeadObject`; `Weak` because the consumer owns the ring.
pub(super) struct ConsumerDeath(Weak<Shared>);

impl crate::DeathRecipient for ConsumerDeath {
    fn binder_died(&self, _who: &crate::WIBinder) {
        if let Some(shared) = self.0.upgrade() {
            shared.dead.store(true, Ordering::SeqCst);
            shared.set_end(Status::from(StatusCode::DeadObject), false);
            // The consumer's own mask: wakes the waiter on this side.
            let _ = shared.flag.wake(NOT_EMPTY);
        }
    }
}

/// A ring endpoint's sink binder, a death link only; RPC-contract calls are logged and dropped.
struct RingSink;

impl Interface for RingSink {}

impl IStreamSink for RingSink {
    fn r#onStart(&self, _source: &SIBinder, _credits: i32) -> BinderResult<()> {
        log::warn!("stream: ignoring onStart on a ring endpoint; the items travel on the ring");
        Ok(())
    }

    fn r#onBatch(&self, _items: &[u8], _count: i32) -> BinderResult<()> {
        log::warn!("stream: ignoring onBatch on a ring endpoint; the items travel on the ring");
        Ok(())
    }

    fn r#onEnd(
        &self,
        _exception: i32,
        _service_specific: i32,
        _message: Option<&str>,
    ) -> BinderResult<()> {
        log::warn!("stream: ignoring onEnd on a ring endpoint; the end travels on the ring");
        Ok(())
    }
}

/// One wait on the pool, this side's only waiter while out: the consumer joins it, not beside.
#[cfg(feature = "tokio")]
struct WaitInTransit {
    shared: Arc<Shared>,
    transit: Arc<Transit>,
    failure: Option<StatusCode>,
}

#[cfg(feature = "tokio")]
impl WaitInTransit {
    fn new(shared: Arc<Shared>, transit: Arc<Transit>) -> Self {
        transit.begin();
        WaitInTransit {
            shared,
            transit,
            failure: None,
        }
    }

    fn wait(mut self) {
        if let Err(e) = self.shared.flag.wait(NOT_EMPTY, None) {
            self.failure = Some(StatusCode::from(e));
        }
    }
}

#[cfg(feature = "tokio")]
impl Drop for WaitInTransit {
    fn drop(&mut self) {
        // Not handed on: the consumer joins this wait before it looks, so it looks after the wake.
        self.transit.settle(self.failure, false, false);
    }
}

/// How long a call waits: the consumer for a record, the producer for room.
#[derive(Clone, Copy)]
enum Wait {
    /// Not at all.
    Never,
    Until(Instant),
    Forever,
}

impl Wait {
    /// A producer call's wait for room: `None` is no deadline, not no wait.
    fn from_deadline(deadline: Option<Instant>) -> Self {
        deadline.map_or(Wait::Forever, Wait::Until)
    }
}

/// What one look at the ring came to.
enum Step<T> {
    Item(T),
    /// The stream is over, with this status.
    End(Status),
    /// Nothing to read and the stream is still running.
    Nothing,
}

/// The consumer over the ring: one record at a time, waiting on `NOT_EMPTY` when empty.
pub(super) struct Consumer<T> {
    shared: Arc<Shared>,
    transit: Arc<Transit>,
    /// The end was reported; later calls read as finished.
    finished: bool,
    sink_binder: SIBinder,
    /// Records copied out of the ring in one go, before anything interprets them; grows up to
    /// the ring's size. `batch[at..filled]` is not parsed yet.
    batch: Vec<u8>,
    at: usize,
    filled: usize,
    /// Scratch for the payload `decode_item` takes.
    buf: Vec<u8>,
    /// Test hook, run at the last point before a call commits to sleeping.
    #[cfg(test)]
    about_to_park: Option<Box<dyn FnMut() + Send>>,
    /// Test hook, run after a look found the ring empty and before that look's verdict.
    #[cfg(test)]
    after_empty_look: Option<Box<dyn FnMut() + Send>>,
    /// Test hook, run between the verdict on an end record and the recording of it.
    #[cfg(test)]
    end_record_read: Option<Box<dyn FnMut() + Send>>,
    _item: PhantomData<fn() -> T>,
}

impl<T: Deserialize> Consumer<T> {
    /// Make the ring and the sink binder; the caller builds the endpoint.
    pub(super) fn new(policy: &ReceiverPolicy) -> Result<(Self, Ring, SIBinder)> {
        // Same bound as `Producer::open`: a header and a byte of payload past the reserve.
        if policy.ring_bytes <= END_RESERVE + HEADER {
            log::error!(
                "Receiver::new: a ring of {} bytes leaves no room for an item past the \
                 {END_RESERVE}-byte reserve for the end record",
                policy.ring_bytes
            );
            return Err(StatusCode::BadValue);
        }
        let queue = MessageQueue::<u8>::create(policy.ring_bytes, true).map_err(|e| {
            log::error!("Receiver::new: the ring cannot be made: {e}");
            StatusCode::from(e)
        })?;
        let capacity = queue.capacity();
        let ring: Ring = queue.descriptor()?.try_into()?;
        // `create(_, true)` makes the word.
        let flag = queue.event_flag().ok_or(StatusCode::InvalidOperation)?;
        let shared = Arc::new(Shared::new(queue, flag, capacity));
        let sink_binder = BnStreamSink::new_binder(RingSink).as_binder();
        let consumer = Consumer {
            shared,
            transit: Arc::new(Transit::default()),
            finished: false,
            sink_binder: sink_binder.clone(),
            batch: Vec::new(),
            at: 0,
            filled: 0,
            buf: Vec::new(),
            #[cfg(test)]
            about_to_park: None,
            #[cfg(test)]
            after_empty_look: None,
            #[cfg(test)]
            end_record_read: None,
            _item: PhantomData,
        };
        Ok((consumer, ring, sink_binder))
    }

    /// The ring described again, with fresh duplicates of its fd.
    pub(super) fn ring(&self) -> Result<Ring> {
        self.shared.queue().descriptor()?.try_into()
    }

    pub(super) fn sink_binder(&self) -> SIBinder {
        self.sink_binder.clone()
    }

    /// A recipient for the producer's death.
    pub(super) fn death_recipient(&self) -> ConsumerDeath {
        ConsumerDeath(Arc::downgrade(&self.shared))
    }

    pub(super) fn recv(&mut self) -> Option<BinderResult<T>> {
        self.next(Wait::Forever)
    }

    pub(super) fn try_recv(&mut self) -> BinderResult<Option<T>> {
        self.next(Wait::Never).transpose()
    }

    pub(super) fn recv_timeout(&mut self, timeout: Duration) -> BinderResult<Option<T>> {
        // A `timeout` no `Instant` can express waits without a bound.
        let wait = match Instant::now().checked_add(timeout) {
            Some(deadline) => Wait::Until(deadline),
            None => Wait::Forever,
        };
        self.next(wait).transpose()
    }

    /// Next item, or the end reported once (an error if bad); `None` after that or on timeout.
    fn next(&mut self, wait: Wait) -> Option<BinderResult<T>> {
        if self.finished {
            return None;
        }
        loop {
            // Join before looking, like the producer: an orphan takes each wake until it settles.
            if !matches!(wait, Wait::Never) && self.transit.in_transit() {
                let deadline = match wait {
                    Wait::Until(deadline) => Some(deadline),
                    _ => None,
                };
                if !self.transit.wait_idle_until(deadline) {
                    return None;
                }
                if let Some(e) = self.transit.take_unreported() {
                    self.fail(Status::from(e));
                }
                continue;
            }
            let deadline = match wait {
                Wait::Never => None,
                Wait::Until(deadline) => Some(deadline),
                Wait::Forever => None,
            };
            // Only a call that may wait: `try_recv` and `recv_async`'s look take what is there.
            if !matches!(wait, Wait::Never) && self.at == self.filled {
                self.let_a_batch_build(deadline);
            }
            match self.step() {
                Step::Item(item) => return Some(Ok(item)),
                Step::End(status) => return self.finish(status),
                Step::Nothing => {}
            }
            if matches!(wait, Wait::Never) {
                return None;
            }
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let timeout = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    Some(left)
                }
                None => None,
            };
            match self.shared.flag.wait(NOT_EMPTY, timeout) {
                Ok(_) => {}
                Err(rsbinder_fmq::Error::TimedOut) => return None,
                Err(e) => {
                    self.fail(Status::from(StatusCode::from(e)));
                }
            }
        }
    }

    #[cfg(feature = "tokio")]
    pub(super) async fn recv_async(&mut self) -> Option<BinderResult<T>> {
        if self.finished {
            return None;
        }
        loop {
            // As in `next`: a wait left on the pool is joined before the look.
            if self.transit.in_transit() {
                self.transit.idle_async().await;
                if let Some(e) = self.transit.take_unreported() {
                    self.fail(Status::from(e));
                }
                continue;
            }
            match self.step() {
                Step::Item(item) => return Some(Ok(item)),
                Step::End(status) => return self.finish(status),
                Step::Nothing => {}
            }
            #[cfg(test)]
            if let Some(hook) = self.about_to_park.as_mut() {
                hook();
            }
            let wait = WaitInTransit::new(self.shared.clone(), self.transit.clone());
            if let Err(e) = on_pool(wait, |wait| {
                wait.wait();
                Ok(())
            })
            .await
            {
                // The pool is gone: no wait can be made, so end rather than spin unwoken.
                self.fail(Status::from(e));
                continue;
            }
            if let Some(e) = self.transit.take_unreported() {
                self.fail(Status::from(e));
            }
        }
    }

    /// One record, if there is one: copied out, then interpreted. Never waits.
    fn step(&mut self) -> Step<T> {
        // Before the look: a death noticed after it may follow an end record the look missed.
        let ended = self.shared.end();
        let (is_end, payload) = match self.read_record() {
            Ok(Some(record)) => record,
            Ok(None) => {
                #[cfg(test)]
                if let Some(hook) = self.after_empty_look.as_mut() {
                    hook();
                }
                // An end recorded since then woke `NOT_EMPTY`, so the next look runs at once.
                return match ended {
                    Some(status) => Step::End(status),
                    None => Step::Nothing,
                };
            }
            Err(what) => return self.corrupted(what),
        };
        if is_end {
            let status = end_status(&self.batch[payload]);
            // The producer's last word outranks only a death notice; one lock, so none slips in.
            let mut end = self.shared.end.lock().unwrap_or_else(|e| e.into_inner());
            let outranks = end
                .as_ref()
                .is_none_or(|end| end.transaction_error() == StatusCode::DeadObject);
            #[cfg(test)]
            if let Some(hook) = self.end_record_read.as_mut() {
                hook();
            }
            if outranks {
                *end = Some(status.clone());
            }
            // What was recorded is the end, as the empty-ring branch reports it.
            return Step::End(end.clone().unwrap_or(status));
        }
        self.buf.clear();
        self.buf.extend_from_slice(&self.batch[payload]);
        match decode_item::<T>(&mut self.buf) {
            Ok(item) => Step::Item(item),
            Err(e) => self.fail(Status::from(e)),
        }
    }

    /// The next record from `batch`, refilled when used up: `Some((is_end, payload range))`,
    /// `None` when both are empty, `Err` on a broken ring.
    fn read_record(
        &mut self,
    ) -> std::result::Result<Option<(bool, std::ops::Range<usize>)>, String> {
        if self.at == self.filled && !self.refill()? {
            return Ok(None);
        }
        let rest = &self.batch[self.at..self.filled];
        if rest.len() < HEADER {
            return Err("a partial record header".to_string());
        }
        let header = u32::from_le_bytes([rest[0], rest[1], rest[2], rest[3]]);
        let is_end = header & KIND_END != 0;
        let len = (header & LEN_MASK) as usize;
        let most = if is_end {
            self.shared.capacity - HEADER
        } else {
            self.shared.capacity - END_RESERVE - HEADER
        };
        if len > most || (is_end && len < END_FIELDS) {
            return Err(format!("a record header claiming {len} bytes"));
        }
        if rest.len() < HEADER + len {
            // A record is committed whole, so a short count means the counters lie.
            return Err(format!(
                "a record of {len} bytes with {} bytes committed",
                rest.len()
            ));
        }
        let payload = self.at + HEADER..self.at + HEADER + len;
        self.at = payload.end;
        Ok(Some((is_end, payload)))
    }

    /// Copy every committed byte out of the ring, free it and wake once: `false` if there were none.
    fn refill(&mut self) -> std::result::Result<bool, String> {
        let mut queue = self.shared.queue();
        let available = queue
            .available_to_read()
            .map_err(|e| format!("the ring's counters: {e}"))?;
        if available == 0 {
            return Ok(false);
        }
        // Grows only, so a refill zeroes nothing it will overwrite anyway.
        if self.batch.len() < available {
            self.batch.resize(available, 0);
        }
        let batch = &mut self.batch[..available];
        queue
            .begin_read(available)
            .map_err(StatusCode::from)
            .and_then(|regions| match regions {
                Some(regions) => regions.read_at(0, batch).map_err(StatusCode::from),
                None => Err(StatusCode::BadValue),
            })
            .and_then(|()| queue.commit_read(available).map_err(StatusCode::from))
            .map_err(|e| format!("the ring's counters: {e:?}"))?;
        drop(queue);
        self.at = 0;
        self.filled = available;
        // The one point room is made; `StreamEndpoint.aidl` lets a multi-record read wake once.
        let _ = self.shared.flag.wake(NOT_FULL);
        Ok(true)
    }

    /// Before a refill, while the producer keeps adding: let up to `MIN_BATCH` bytes build up,
    /// for at most the spin budget. Stops as soon as a round of looks sees no growth.
    fn let_a_batch_build(&self, deadline: Option<Instant>) {
        let budget = spin_budget(deadline);
        if budget.is_zero() {
            return;
        }
        let queue = self.shared.queue();
        // A broken counter is left for the refill to report.
        let Ok(mut seen) = queue.available_to_read() else {
            return;
        };
        if seen == 0 || seen >= MIN_BATCH {
            return;
        }
        let start = Instant::now();
        loop {
            for _ in 0..SPIN_CLOCK_EVERY {
                std::hint::spin_loop();
            }
            match queue.available_to_read() {
                Ok(now) if now > seen && now < MIN_BATCH && start.elapsed() < budget => seen = now,
                _ => return,
            }
        }
    }

    /// The ring broke its contract: end with `EX_ILLEGAL_STATE` and stop the producer.
    fn corrupted(&mut self, what: String) -> Step<T> {
        log::error!("stream: {what}");
        self.fail(Status::from((
            ExceptionCode::IllegalState,
            format!("the stream's ring is corrupted: {what}").as_str(),
        )))
    }

    /// End the stream here, overriding any earlier end, and set `CANCEL` for the producer.
    fn fail(&mut self, status: Status) -> Step<T> {
        self.shared.set_end(status.clone(), true);
        let _ = self.shared.flag.wake(CANCEL);
        Step::End(status)
    }

    /// The end has been reached: report it once, then read as finished.
    fn finish(&mut self, status: Status) -> Option<BinderResult<T>> {
        self.finished = true;
        if status.is_ok() {
            None
        } else {
            Some(Err(status))
        }
    }
}

impl<T> Consumer<T> {
    pub(super) fn is_finished(&self) -> bool {
        self.finished
    }

    pub(super) fn end_status(&self) -> Option<Status> {
        // A death notice can land before the ring is drained; it is the end only once read as one.
        self.finished.then(|| self.shared.end()).flatten()
    }

    /// Set `CANCEL`; the producer sees it before its next write or in the wait it is parked in.
    pub(super) fn cancel(&self) -> Result<()> {
        self.shared.flag.wake(CANCEL)?;
        Ok(())
    }
}

impl<T> Drop for Consumer<T> {
    fn drop(&mut self) {
        // A producer parked on a full ring has no other way to learn nobody reads any more.
        let _ = self.shared.flag.wake(CANCEL);
        if self.transit.in_transit() {
            // Wake an orphaned pool wait with this side's own mask so its thread is given back.
            let _ = self.shared.flag.wake(NOT_EMPTY);
        }
    }
}

// `MessageQueue::create` is `Unsupported` off Linux/Android.
#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;
    #[cfg(feature = "tokio")]
    use std::future::Future;
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc;
    use std::thread;

    fn receiver_policy(ring_bytes: usize) -> ReceiverPolicy {
        ReceiverPolicy {
            ring_bytes,
            ..ReceiverPolicy::default()
        }
    }

    /// A producer and a consumer on one ring in one process.
    fn pair<T: Serialize + Deserialize>(ring_bytes: usize) -> (Producer<T>, Consumer<T>) {
        let (rx, ring, sink) = Consumer::<T>::new(&receiver_policy(ring_bytes)).expect("a ring");
        let tx = Producer::<T>::open(&ring, &sink, &SinkPolicy::default()).expect("attach");
        (tx, rx)
    }

    fn end<T: Serialize + ?Sized>(tx: Producer<T>) -> Result<()> {
        tx.terminate(None, ExceptionCode::None as i32, 0, None)
    }

    /// How many eight-byte `i32` records fill the part of the ring items may use.
    fn item_records(ring_bytes: usize) -> usize {
        (ring_bytes - END_RESERVE) / 8
    }

    /// The producer ends cleanly and dies after the consumer's look found nothing: the end is read.
    #[test]
    fn an_end_committed_before_a_death_noticed_after_the_look_is_read() {
        let (tx, mut rx) = pair::<i32>(512);
        let shared = rx.shared.clone();
        let mut tx = Some(tx);
        rx.after_empty_look = Some(Box::new(move || {
            if let Some(tx) = tx.take() {
                end(tx).expect("the end record");
                // What `ConsumerDeath` does once the producer's process is gone.
                shared.dead.store(true, Ordering::SeqCst);
                shared.set_end(Status::from(StatusCode::DeadObject), false);
                let _ = shared.flag.wake(NOT_EMPTY);
            }
        }));
        assert!(rx.recv().is_none(), "a clean end, not DeadObject");
        assert!(rx.shared.end().expect("ended").is_ok());
    }

    #[test]
    fn records_cross_the_ring_in_order_and_the_end_is_last() {
        // 512 bytes hold 32 item records, so 200 items wrap the ring several times.
        let (mut tx, mut rx) = pair::<i32>(512);
        let producer = thread::spawn(move || {
            for item in 0..200i32 {
                tx.send(&item, None).expect("send");
            }
            end(tx).expect("end");
        });
        let mut got = Vec::new();
        while let Some(item) = rx.recv() {
            got.push(item.expect("item"));
        }
        assert_eq!(got, (0..200).collect::<Vec<_>>());
        assert!(rx.is_finished());
        assert!(rx.end_status().expect("ended").is_ok());
        assert!(rx.recv().is_none(), "over is over");
        producer.join().expect("producer");
    }

    #[test]
    fn an_item_larger_than_the_ring_allows_is_refused_and_not_written() {
        let (mut tx, mut rx) = pair::<Vec<u8>>(1024);
        // A `Vec<u8>` encodes as a length and the bytes, padded to four.
        let too_big = vec![0u8; 1024 - END_RESERVE];
        assert_eq!(tx.send(&too_big, None).err(), Some(StatusCode::BadValue));
        assert!(
            tx.scratch.capacity() < too_big.len(),
            "the scratch does not keep what the refused item grew"
        );
        let fits = vec![7u8; 700];
        tx.send(&fits, None).expect("an item within the limit");
        end(tx).expect("end");
        assert_eq!(rx.recv().expect("the item").expect("ok"), fits);
        assert!(rx.recv().is_none(), "the refused item never went in");
    }

    /// Plan 10-7b AC-7b.2, in one process: the producer stops at item capacity until reads.
    #[test]
    fn a_full_ring_stops_the_producer_until_the_consumer_reads() {
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512);
        let sent = Arc::new(AtomicUsize::new(0));
        let counter = sent.clone();
        let producer = thread::spawn(move || {
            for item in 0..1000i32 {
                tx.send(&item, None).expect("send");
                counter.fetch_add(1, Ordering::SeqCst);
            }
            end(tx).expect("end");
        });

        let deadline = Instant::now() + Duration::from_secs(5);
        while sent.load(Ordering::SeqCst) < fill {
            assert!(Instant::now() < deadline, "the producer must fill the ring");
            thread::sleep(Duration::from_millis(5));
        }
        thread::sleep(Duration::from_millis(100));
        assert_eq!(
            sent.load(Ordering::SeqCst),
            fill,
            "the ring's item capacity is the whole budget"
        );

        let mut got = Vec::new();
        while let Some(item) = rx.recv() {
            got.push(item.expect("item"));
        }
        assert_eq!(got, (0..1000).collect::<Vec<_>>());
        producer.join().expect("producer");
    }

    /// Plan 10-7b AC-7b.7: the reserve lets `end` and `Drop` write into a ring full of items.
    #[test]
    fn the_end_record_goes_in_without_waiting_when_the_ring_is_full() {
        for dropped in [false, true] {
            let (mut tx, mut rx) = pair::<i32>(512);
            for item in 0..item_records(512) as i32 {
                tx.send(&item, None).expect("fills the ring");
            }
            let (done, watch) = mpsc::channel();
            thread::spawn(move || {
                let outcome = if dropped {
                    drop(tx);
                    Ok(())
                } else {
                    end(tx)
                };
                let _ = done.send(outcome);
            });
            watch
                .recv_timeout(Duration::from_secs(5))
                .expect("the end record must not wait for the consumer")
                .expect("end");

            let mut got = 0;
            let ended = loop {
                match rx.recv() {
                    Some(Ok(_)) => got += 1,
                    Some(Err(status)) => break Some(status),
                    None => break None,
                }
            };
            assert_eq!(got, item_records(512), "dropped {dropped}");
            match ended {
                None => assert!(!dropped, "a dropped producer ends the stream badly"),
                Some(status) => {
                    assert!(dropped);
                    assert_eq!(status.exception_code(), ExceptionCode::IllegalState);
                    assert!(
                        status
                            .message()
                            .unwrap_or_default()
                            .contains("0 queued item"),
                        "nothing was lost: every item was in the ring: {status:?}"
                    );
                }
            }
        }
    }

    /// Plan 10-7b AC-7b.5: a cancel releases a producer parked on a full ring; later sends fail.
    #[test]
    fn a_cancel_releases_a_producer_parked_on_a_full_ring() {
        let (mut tx, rx) = pair::<i32>(512);
        let (outcome, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            let mut sent = 0;
            let parked = loop {
                match tx.send(&sent, None) {
                    Ok(()) => sent += 1,
                    Err(e) => break e,
                }
            };
            let _ = outcome.send((sent, parked, tx.is_canceled(), tx.send(&0, None)));
        });
        let full = item_records(512) * 8;
        let deadline = Instant::now() + Duration::from_secs(5);
        while rx.shared.queue().available_to_read().expect("counters") < full {
            assert!(Instant::now() < deadline, "the producer must fill the ring");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            watch.try_recv().is_err(),
            "the producer must be parked on the full ring"
        );
        rx.cancel().expect("cancel");
        let (sent, parked, canceled, next) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("cancel must release the producer");
        assert_eq!(sent as usize, item_records(512));
        assert_eq!(parked, StatusCode::InvalidOperation);
        assert!(
            canceled,
            "`is_canceled` tells the refusal from a broken ring"
        );
        assert_eq!(next.err(), Some(StatusCode::InvalidOperation));
        producer.join().expect("producer");
    }

    /// Dropping the consumer cancels too; an unparked producer sees it before its next write.
    #[test]
    fn dropping_the_consumer_cancels_the_producer() {
        let (mut tx, rx) = pair::<i32>(512);
        tx.send(&1, None).expect("send");
        drop(rx);
        assert!(tx.is_canceled());
        assert_eq!(tx.send(&2, None).err(), Some(StatusCode::InvalidOperation));
        // The ring outlives the consumer's mapping, so the terminator goes in and succeeds.
        end(tx).expect("end after cancel");
    }

    /// Plan 10-7b AC-7b.4, the producer's half: death releases a producer parked on a full ring.
    #[test]
    fn a_dead_consumer_releases_a_parked_producer() {
        let (mut tx, _rx) = pair::<i32>(512);
        let death = ProducerDeath(tx.shared.clone());
        let (outcome, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            let parked = loop {
                if let Err(e) = tx.send(&0, None) {
                    break e;
                }
            };
            let _ = outcome.send((parked, end(tx)));
        });
        assert!(watch.recv_timeout(Duration::from_millis(300)).is_err());
        crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&_rx.sink_binder()));
        let (parked, ended) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("death must release the producer");
        assert_eq!(parked, StatusCode::DeadObject);
        assert_eq!(ended.err(), Some(StatusCode::DeadObject), "nobody to tell");
        producer.join().expect("producer");
    }

    /// AC-7b.4, the consumer's half: death ends a wait on an empty ring, once the ring is drained.
    #[test]
    fn a_dead_producer_releases_a_blocked_consumer_after_what_it_wrote() {
        let (mut tx, mut rx) = pair::<i32>(512);
        tx.send(&1, None).expect("send");
        let death = rx.death_recipient();
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let first = rx.recv();
            let second = rx.recv();
            let third = rx.recv();
            let _ = done.send((first, second, third, rx));
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the consumer waits for a second record"
        );
        crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&tx.sink));
        let (first, second, third, _rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("death must release the consumer");
        assert_eq!(first.expect("the item").expect("ok"), 1);
        assert_eq!(
            second
                .expect("the end")
                .expect_err("an error")
                .transaction_error(),
            StatusCode::DeadObject
        );
        assert!(third.is_none(), "reported once");
    }

    /// A death notice after the end record was written must not turn a clean end into `DeadObject`.
    #[test]
    fn an_end_record_outranks_a_death_that_followed_it() {
        let (tx, mut rx) = pair::<i32>(512);
        let death = rx.death_recipient();
        let sink = tx.sink.clone();
        end(tx).expect("end");
        crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&sink));
        assert!(rx.recv().is_none());
        assert!(rx.end_status().expect("ended").is_ok());
    }

    /// No death notice can be recorded between the verdict on an end record and its recording.
    #[test]
    fn an_end_record_outranks_a_death_noticed_while_it_is_being_read() {
        let (tx, mut rx) = pair::<i32>(512);
        let death = rx.death_recipient();
        let sink = tx.sink.clone();
        let shared = rx.shared.clone();
        let reached = Arc::new(AtomicBool::new(false));
        let seen = reached.clone();
        end(tx).expect("end");
        rx.end_record_read = Some(Box::new(move || {
            seen.store(true, Ordering::SeqCst);
            // `binder_died` records under `end`: a free lock here is a window it could slip into.
            let window_open = shared.end.try_lock().is_ok();
            if window_open {
                crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&sink));
            }
        }));
        assert!(rx.recv().is_none(), "a clean end");
        assert!(reached.load(Ordering::SeqCst), "the hook sat on the path");
        assert!(rx.end_status().expect("ended").is_ok(), "not DeadObject");
    }

    /// A failure recorded here before the look is what `recv` reports over a clean end record.
    #[test]
    fn a_failure_recorded_before_a_clean_end_record_is_the_reported_end() {
        let (tx, mut rx) = pair::<i32>(512);
        end(tx).expect("end");
        // What `next` and `recv_async` do on a failed wait before they look again.
        let _ = rx.fail(Status::from(StatusCode::InvalidOperation));
        let ended = rx.recv().expect("the end").expect_err("an error");
        assert_eq!(ended.transaction_error(), StatusCode::InvalidOperation);
        let status = rx.end_status().expect("ended");
        assert_eq!(status.transaction_error(), StatusCode::InvalidOperation);
    }

    #[test]
    fn a_death_notice_is_no_end_status_while_records_remain() {
        let (mut tx, mut rx) = pair::<i32>(512);
        tx.send(&1, None).expect("send");
        let death = rx.death_recipient();
        crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&tx.sink));
        assert!(rx.end_status().is_none(), "an item is still in the ring");
        assert_eq!(rx.recv().expect("the item").expect("ok"), 1);
        let ended = rx.recv().expect("the end").expect_err("an error");
        assert_eq!(ended.transaction_error(), StatusCode::DeadObject);
        assert_eq!(
            rx.end_status().expect("ended").transaction_error(),
            StatusCode::DeadObject
        );
    }

    /// A writer handle outside the producer's checks, which is what a hostile peer amounts to.
    fn raw_writer(ring: &Ring) -> MessageQueue<u8> {
        let policy = AttachPolicy {
            max_capacity: 512,
            require_seal: true,
            require_event_flag: true,
        };
        MessageQueue::<u8>::attach(&Descriptor::try_from(ring).expect("desc"), &policy)
            .expect("attach")
    }

    /// One refill takes every committed record and wakes once; records written later follow.
    #[test]
    fn a_refill_takes_every_committed_record_and_wakes_once() {
        let (mut tx, mut rx) = pair::<i32>(512);
        for item in 0..3 {
            tx.send(&item, None).expect("send");
        }
        assert_eq!(rx.try_recv().expect("no error"), Some(0));
        assert_eq!(
            rx.shared.queue().available_to_read().expect("counters"),
            0,
            "the refill freed all three records"
        );
        let flag = &rx.shared.flag;
        assert_eq!(flag.wait(NOT_FULL, Some(Duration::ZERO)), Ok(NOT_FULL));
        assert_eq!(rx.try_recv().expect("no error"), Some(1));
        assert_eq!(
            rx.shared.flag.peek() & NOT_FULL,
            0,
            "a record from the batch frees nothing, so wakes nobody"
        );
        for item in 3..5 {
            tx.send(&item, None).expect("send");
        }
        for expected in 2..5 {
            assert_eq!(rx.try_recv().expect("no error"), Some(expected));
        }
        assert_eq!(rx.try_recv().expect("no error"), None);
    }

    /// An end record inside the batch comes after the items before it, and only then.
    #[test]
    fn an_end_record_inside_the_batch_follows_the_items_before_it() {
        let (mut tx, mut rx) = pair::<i32>(512);
        tx.send(&1, None).expect("send");
        tx.send(&2, None).expect("send");
        end(tx).expect("end");
        assert_eq!(rx.try_recv().expect("no error"), Some(1));
        assert!(!rx.is_finished(), "the end is in the batch, not reached");
        assert_eq!(rx.try_recv().expect("no error"), Some(2));
        assert_eq!(rx.try_recv().expect("no error"), None);
        assert!(rx.is_finished());
        assert!(rx.end_status().expect("ended").is_ok());
    }

    /// Records copied out before a death notice are still delivered, then the death is the end.
    #[test]
    fn a_death_noticed_after_a_refill_ends_the_stream_after_the_batch() {
        let (mut tx, mut rx) = pair::<i32>(512);
        for item in 1..=3 {
            tx.send(&item, None).expect("send");
        }
        assert_eq!(rx.try_recv().expect("no error"), Some(1));
        let death = rx.death_recipient();
        crate::DeathRecipient::binder_died(&death, &SIBinder::downgrade(&tx.sink));
        assert_eq!(rx.recv().expect("item").expect("ok"), 2);
        assert_eq!(rx.recv().expect("item").expect("ok"), 3);
        let ended = rx.recv().expect("the end").expect_err("an error");
        assert_eq!(ended.transaction_error(), StatusCode::DeadObject);
    }

    /// A bad header behind a good record: the item first, then `EX_ILLEGAL_STATE` and `CANCEL`.
    #[test]
    fn a_bad_header_inside_the_batch_ends_the_stream_after_the_items_before_it() {
        let (mut rx, ring, _sink) = Consumer::<i32>::new(&receiver_policy(512)).expect("a ring");
        let mut raw = raw_writer(&ring);
        let mut records = item_header(4).to_le_bytes().to_vec();
        records.extend_from_slice(&7i32.to_le_bytes());
        records.extend_from_slice(&item_header(512).to_le_bytes());
        records.extend_from_slice(&[0; 4]);
        assert!(raw.write(&records).expect("write"));

        assert_eq!(rx.try_recv().expect("no error"), Some(7));
        let failure = rx.try_recv().expect_err("the stream must end");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(rx.shared.flag.peek() & CANCEL != 0);
        assert!(rx.try_recv().expect("over").is_none());
    }

    /// A record is committed whole: a header committed without its payload is a broken ring.
    #[test]
    fn a_record_committed_in_two_parts_ends_the_stream() {
        let (mut rx, ring, _sink) = Consumer::<i32>::new(&receiver_policy(512)).expect("a ring");
        let mut raw = raw_writer(&ring);
        assert!(raw.write(&item_header(4).to_le_bytes()).expect("write"));
        let failure = rx.try_recv().expect_err("the stream must end");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(
            failure.message().unwrap_or_default().contains("committed"),
            "{failure:?}"
        );
    }

    /// A producer that writes one item and stops: the wait for a batch gives up at once.
    #[test]
    fn a_lone_item_is_not_held_back_for_a_batch() {
        let (mut tx, mut rx) = pair::<i32>(64 * 1024);
        for item in 0..20 {
            tx.send(&item, None).expect("send");
            let started = Instant::now();
            assert_eq!(
                rx.recv_timeout(Duration::from_secs(5)).expect("no error"),
                Some(item)
            );
            let took = started.elapsed();
            assert!(
                took < SPIN + Duration::from_millis(5),
                "item {item} took {took:?}"
            );
        }
    }

    /// Plan 10-7b AC-7b.6: an oversized header ends as `EX_ILLEGAL_STATE` and sets `CANCEL`.
    #[test]
    fn a_record_header_the_ring_cannot_hold_ends_the_stream() {
        let (rx, ring, _sink) = Consumer::<i32>::new(&receiver_policy(512)).expect("a ring");
        let mut rx = rx;
        // A second writer outside the producer's checks is what a hostile peer amounts to.
        let policy = AttachPolicy {
            max_capacity: 512,
            require_seal: true,
            require_event_flag: true,
        };
        let mut raw =
            MessageQueue::<u8>::attach(&Descriptor::try_from(&ring).expect("desc"), &policy)
                .expect("attach");
        let bogus = item_header(512 - END_RESERVE);
        assert!(raw.write(&bogus.to_le_bytes()).expect("write"));
        assert!(raw.write(&[0, 0, 0, 0]).expect("write"));

        let failure = rx.try_recv().expect_err("the stream must end");
        assert_eq!(failure.exception_code(), ExceptionCode::IllegalState);
        assert!(
            rx.shared.flag.peek() & CANCEL != 0,
            "the producer is told the one way it listens"
        );
        assert!(rx.try_recv().expect("over").is_none());
    }

    /// A payload that is not one `T` is a decode failure, reported once, with `CANCEL` set.
    #[test]
    fn a_record_that_does_not_decode_ends_the_stream() {
        let (tx, mut rx) = pair::<i64>(512);
        // Two bytes cannot be an `i64`.
        assert!(tx
            .shared
            .write_record(item_header(2), &[1, 2], Wait::Never)
            .expect("write"));
        assert!(rx.recv().expect("a report").is_err());
        assert!(rx.recv().is_none());
        assert!(tx.is_canceled());
    }

    #[test]
    fn the_end_message_is_cut_to_the_reserve_on_a_character_boundary() {
        let (tx, mut rx) = pair::<i32>(512);
        // Three bytes a character and 244 is not a multiple of three, so the cut steps back.
        let long: String = "가".repeat(200);
        tx.terminate(None, ExceptionCode::ServiceSpecific as i32, 42, Some(&long))
            .expect("end_with");
        let failure = rx.recv().expect("the failure").expect_err("an error");
        assert_eq!(failure.exception_code(), ExceptionCode::ServiceSpecific);
        assert_eq!(failure.service_specific_error(), 42);
        let message = failure.message().expect("a message");
        assert_eq!(message.len(), 243);
        assert!(long.starts_with(message));
        assert!(rx.recv().is_none());
    }

    #[test]
    fn a_ring_no_larger_than_the_reserve_is_refused() {
        assert_eq!(
            Consumer::<i32>::new(&receiver_policy(END_RESERVE + HEADER)).err(),
            Some(StatusCode::BadValue)
        );
        assert!(Consumer::<i32>::new(&receiver_policy(END_RESERVE + HEADER + 1)).is_ok());
    }

    #[test]
    fn a_ring_wider_than_the_producer_accepts_is_refused_at_attach() {
        let (_rx, ring, sink) = Consumer::<i32>::new(&receiver_policy(8192)).expect("a ring");
        let policy = SinkPolicy {
            max_ring_bytes: 4096,
            ..SinkPolicy::default()
        };
        assert_eq!(
            Producer::<i32>::open(&ring, &sink, &policy).err(),
            Some(StatusCode::BadValue)
        );
    }

    /// `recv_timeout` bounds the wait and leaves the stream usable.
    #[test]
    fn a_timeout_leaves_the_stream_running() {
        let (mut tx, mut rx) = pair::<i32>(512);
        let before = Instant::now();
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(100))
                .expect("no error"),
            None
        );
        assert!(before.elapsed() >= Duration::from_millis(100));
        assert!(!rx.is_finished());
        tx.send(&5, None).expect("send");
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(5)).expect("no error"),
            Some(5)
        );
        assert_eq!(rx.try_recv().expect("no error"), None);
    }

    /// A parked async producer must yield the thread, or a current-thread runtime deadlocks.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_async_pair_finishes_on_a_current_thread_runtime() {
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .build()
                .expect("runtime");
            let got = runtime.block_on(async {
                let (mut tx, mut rx) = pair::<i32>(512);
                // `tokio::spawn` also checks that the futures are `Send`.
                let producer = tokio::spawn(async move {
                    for item in 0..500i32 {
                        tx.send_async(&item, None).await?;
                    }
                    tx.terminate_async(ExceptionCode::None as i32, 0, None)
                        .await
                });
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
            (0..500).collect::<Vec<_>>()
        );
    }

    /// A live `recv_async` wait consumes its wake for good and leaves `NOT_EMPTY` clear.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_finished_recv_async_wait_leaves_the_futex_clear() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let got = runtime.block_on(async {
            let mut pending = std::pin::pin!(rx.recv_async());
            // One poll: nothing to read, so the wait goes to the pool.
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled);
            tx.send(&7, None).expect("send");
            pending.await
        });
        assert_eq!(got.expect("an item").expect("ok"), 7);
        assert_eq!(rx.shared.flag.peek() & NOT_EMPTY, 0, "the wake is consumed");
        assert_eq!(rx.try_recv().expect("no error"), None);
    }

    /// A `recv_async` dropped mid-wait leaves an orphan; the next call joins it and loses no wake.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_dropped_recv_async_does_not_lose_the_next_record() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        runtime.block_on(async {
            let mut pending = std::pin::pin!(rx.recv_async());
            // One poll: nothing to read, so the wait goes to the pool.
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled);
        });
        assert!(rx.transit.in_transit(), "the orphan is on the pool");

        // The consumer parks first, so the orphan takes the write's wake.
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let got = rx.recv_timeout(Duration::from_secs(5));
            let _ = done.send((got, rx));
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the consumer must be parked behind the orphan"
        );
        tx.send(&9, None).expect("send");
        let (got, mut rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the write must release the consumer");
        assert_eq!(got.expect("no error"), Some(9));
        let deadline = Instant::now() + Duration::from_secs(5);
        while rx.transit.in_transit() {
            assert!(Instant::now() < deadline, "the orphan must settle");
            thread::sleep(Duration::from_millis(5));
        }

        // And the async call, with the orphan made by a sync-side drop.
        runtime.block_on(async {
            let mut pending = std::pin::pin!(rx.recv_async());
            let _ =
                std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx))).await;
        });
        let (done, watch) = mpsc::channel();
        let handle = runtime.handle().clone();
        thread::spawn(move || {
            let got = handle.block_on(rx.recv_async());
            let _ = done.send((got, rx));
        });
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the async consumer must be parked behind the orphan"
        );
        tx.send(&10, None).expect("send");
        let (got, rx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the write must release the async consumer");
        assert_eq!(got.expect("an item").expect("ok"), 10);
        drop(rx);
        // Dropping the consumer releases the orphan too, or shutdown waits on a parked thread.
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// Leave one `recv_async` wait on the pool.
    #[cfg(feature = "tokio")]
    fn park_an_orphan(runtime: &tokio::runtime::Runtime, rx: &mut Consumer<i32>) {
        runtime.block_on(async {
            let mut pending = std::pin::pin!(rx.recv_async());
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "nothing to read, so the wait goes to the pool");
        });
        assert!(rx.transit.in_transit(), "the orphan is on the pool");
    }

    /// §10.3's window: `recv` is held before sleeping; a record lands and an orphan takes the wake.
    #[cfg(feature = "tokio")]
    fn a_record_lands_in_the_window(
        mut rx: Consumer<i32>,
        mut tx: Producer<i32>,
        recv: impl FnOnce(&mut Consumer<i32>) -> Option<i32> + Send + 'static,
    ) -> (Option<i32>, Consumer<i32>) {
        let (in_window, window) = mpsc::channel();
        let (release, released) = mpsc::channel::<()>();
        rx.about_to_park = Some(Box::new(move || {
            let _ = in_window.send(());
            let _ = released.recv_timeout(Duration::from_secs(5));
        }));
        let transit = rx.transit.clone();
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let got = recv(&mut rx);
            let _ = done.send((got, rx));
        });
        // Either held in the window or joined the orphan first; the record goes in either way.
        let held = window.recv_timeout(Duration::from_millis(500)).is_ok();
        tx.send(&9, None).expect("send");
        if held {
            let deadline = Instant::now() + Duration::from_secs(5);
            while transit.in_transit() {
                assert!(
                    Instant::now() < deadline,
                    "the orphan must take the wake and settle"
                );
                thread::sleep(Duration::from_millis(1));
            }
            release
                .send(())
                .expect("the consumer is waiting in the hook");
        }
        let (got, mut rx) = watch
            .recv_timeout(Duration::from_secs(10))
            .expect("the consumer call must return");
        rx.about_to_park = None;
        (got, rx)
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn a_record_that_lands_after_the_look_is_not_slept_through_by_recv() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (tx, mut rx) = pair::<i32>(512);
        park_an_orphan(&runtime, &mut rx);
        let (got, rx) = a_record_lands_in_the_window(rx, tx, |rx| {
            rx.recv_timeout(Duration::from_secs(2)).expect("no error")
        });
        assert_eq!(
            got,
            Some(9),
            "the record is in the ring; the call must not sleep past it"
        );
        drop(rx);
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    #[cfg(feature = "tokio")]
    #[test]
    fn a_record_that_lands_after_the_look_is_not_slept_through_by_recv_async() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (tx, mut rx) = pair::<i32>(512);
        park_an_orphan(&runtime, &mut rx);
        let handle = runtime.handle().clone();
        // Asleep past the record, this thread is held; the helper's bounded wait fails the test.
        let (got, rx) = a_record_lands_in_the_window(rx, tx, move |rx| {
            handle
                .block_on(rx.recv_async())
                .map(|item| item.expect("ok"))
        });
        assert_eq!(
            got,
            Some(9),
            "the record is in the ring; the call must not sleep past it"
        );
        drop(rx);
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// A dropped `send_async` keeps its place on the pool; `Drop` gives it up rather than wait.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_dropped_send_async_keeps_its_place_and_is_given_up_on_drop() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        runtime.block_on(async {
            let mut pending = std::pin::pin!(tx.send_async(&fill, None));
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "no room, so the record went to the pool");
        });
        assert_eq!(tx.pending(), 1);

        // One read moves every record into the consumer's batch: the orphan goes in first.
        assert_eq!(rx.recv().expect("item").expect("ok"), 0);
        let (done, watch) = mpsc::channel();
        let producer = thread::spawn(move || {
            for item in fill + 1..=2 * fill {
                let _ = done.send(tx.send(&item, None));
            }
            tx
        });
        // The orphan holds one record's room, so the last of these finds the ring full.
        for _ in 1..fill {
            watch
                .recv_timeout(Duration::from_secs(5))
                .expect("room")
                .expect("send");
        }
        assert!(
            watch.recv_timeout(Duration::from_millis(300)).is_err(),
            "the ring is full again, so the next send parks"
        );
        for expected in 1..2 * fill {
            assert_eq!(rx.recv().expect("item").expect("ok"), expected);
        }
        watch
            .recv_timeout(Duration::from_secs(5))
            .expect("released")
            .expect("send");
        let mut tx = producer.join().expect("producer");
        assert_eq!(rx.recv().expect("item").expect("ok"), 2 * fill);

        // A record on the pool, the ring full, then the producer dropped: `Drop` must not wait.
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        runtime.block_on(async {
            let mut pending = std::pin::pin!(tx.send_async(&fill, None));
            let _ =
                std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx))).await;
        });
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            drop(tx);
            let _ = done.send(());
        });
        watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the drop must not wait for the consumer");
        let mut got = 0;
        let ended = loop {
            match rx.recv() {
                Some(Ok(_)) => got += 1,
                Some(Err(status)) => break status,
                None => panic!("a dropped producer ends the stream badly"),
            }
        };
        assert_eq!(got, fill);
        assert!(
            ended
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the given-up record is reported: {ended:?}"
        );
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// A `send_async` dropped behind an orphan loses its item, and the terminator counts it.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_send_async_dropped_behind_an_orphan_is_counted_lost() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        runtime.block_on(async {
            for item in [fill, fill + 1] {
                let mut pending = std::pin::pin!(tx.send_async(&item, None));
                let polled = std::future::poll_fn(|cx| {
                    std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
                })
                .await;
                assert!(polled, "item {item} waits");
            }
        });
        assert_eq!(tx.pending(), 1, "only the first record is on the pool");
        assert_eq!(tx.transit.lost(), 1, "the second item is counted");

        let producer = thread::spawn(move || end(tx));
        for expected in 0..=fill {
            assert_eq!(rx.recv().expect("item").expect("ok"), expected);
        }
        let ended = rx.recv().expect("the end").expect_err("not a clean end");
        assert!(
            ended
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the dropped item is reported: {ended:?}"
        );
        producer.join().expect("producer").expect("end");
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// An awaited `send_async` that times out on the pool: the thread is back, the item not lost.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_send_async_that_times_out_returns_its_pool_thread_and_loses_nothing() {
        let timeout = Duration::from_millis(50);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        let (done, watch) = mpsc::channel();
        let handle = runtime.handle().clone();
        thread::spawn(move || {
            let before = Instant::now();
            let sent = handle.block_on(tx.send_async(&-1, Some(timeout)));
            let _ = done.send((sent, before.elapsed(), tx));
        });
        let (sent, elapsed, mut tx) = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the timeout must end the pool's wait");
        assert_eq!(sent.err(), Some(StatusCode::TimedOut));
        assert!(elapsed >= timeout, "returned after {elapsed:?}");
        assert!(!tx.transit.in_transit(), "the pool task has finished");
        assert_eq!(
            tx.transit.lost(),
            0,
            "reported to its caller, so not counted lost"
        );

        for expected in 0..fill {
            assert_eq!(rx.try_recv().expect("no error"), Some(expected));
        }
        assert_eq!(
            rx.try_recv().expect("no error"),
            None,
            "the item is not in the ring"
        );
        tx.send(&fill, None).expect("room again");
        end(tx).expect("end");
        assert_eq!(rx.recv().expect("the item").expect("ok"), fill);
        assert!(rx.recv().is_none());
        assert!(rx.end_status().expect("ended").is_ok());
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// A dropped `send_async`'s record keeps its deadline on the pool, then settles as lost.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_dropped_send_async_gives_its_pool_thread_back_at_the_deadline() {
        let timeout = Duration::from_millis(50);
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        let before = Instant::now();
        runtime.block_on(async {
            let mut pending = std::pin::pin!(tx.send_async(&-1, Some(timeout)));
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "no room, so the record went to the pool");
        });
        assert!(
            tx.transit
                .wait_idle_until(Some(Instant::now() + Duration::from_secs(5))),
            "the pool's wait must end at the deadline, with nobody reading"
        );
        assert!(before.elapsed() >= timeout);
        assert_eq!(
            tx.transit.lost(),
            1,
            "nobody was told, so the terminator counts it"
        );

        let producer = thread::spawn(move || end(tx));
        for expected in 0..fill {
            assert_eq!(rx.recv().expect("item").expect("ok"), expected);
        }
        let ended = rx.recv().expect("the end").expect_err("not a clean end");
        assert!(
            ended
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the timed-out record is reported: {ended:?}"
        );
        assert_eq!(
            producer.join().expect("producer").err(),
            Some(StatusCode::TimedOut),
            "`end` reports the failure the record left"
        );
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// What a dropped `send_async` left is returned by the next `send`, though the ring has room.
    #[cfg(feature = "tokio")]
    #[test]
    fn a_failure_a_dropped_send_async_left_is_returned_by_the_next_send() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        runtime.block_on(async {
            let mut pending = std::pin::pin!(tx.send_async(&-1, Some(Duration::from_millis(50))));
            let polled = std::future::poll_fn(|cx| {
                std::task::Poll::Ready(pending.as_mut().poll(cx).is_pending())
            })
            .await;
            assert!(polled, "no room, so the record went to the pool");
        });
        assert!(tx
            .transit
            .wait_idle_until(Some(Instant::now() + Duration::from_secs(5))));
        assert!(
            !tx.transit.quiet(),
            "the settled failure keeps `send` off the fast path"
        );
        assert_eq!(rx.recv().expect("item").expect("ok"), 0);
        assert_eq!(
            tx.send(&fill, None).err(),
            Some(StatusCode::TimedOut),
            "the failure is reported before a write that now fits"
        );
        tx.send(&fill, None).expect("reported once");
        end(tx).expect("the end fits the reserve");
        for expected in 1..=fill {
            assert_eq!(rx.recv().expect("item").expect("ok"), expected);
        }
        let ended = rx.recv().expect("the end").expect_err("not a clean end");
        assert!(
            ended
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "the timed-out record is still counted: {ended:?}"
        );
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    /// `end` past its deadline gives up the dropped future's record and still ends the stream.
    #[cfg(feature = "tokio")]
    #[test]
    fn an_end_past_its_deadline_gives_up_the_record_in_transit() {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .build()
            .expect("runtime");
        let (mut tx, mut rx) = pair::<i32>(512);
        let fill = item_records(512) as i32;
        for item in 0..fill {
            tx.send(&item, None).expect("fills the ring");
        }
        // An unbounded record on the pool, so only `end`'s own deadline can end the wait.
        runtime.block_on(async {
            let mut pending = std::pin::pin!(tx.send_async(&-1, None));
            let _ =
                std::future::poll_fn(|cx| std::task::Poll::Ready(pending.as_mut().poll(cx))).await;
        });
        assert!(tx.transit.in_transit());
        let (done, watch) = mpsc::channel();
        thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_millis(50);
            let _ = done.send(tx.terminate(Some(deadline), ExceptionCode::None as i32, 0, None));
        });
        let ended = watch
            .recv_timeout(Duration::from_secs(5))
            .expect("the deadline must end the wait for the record");
        assert_eq!(ended.err(), Some(StatusCode::TimedOut));
        let mut got = 0;
        let status = loop {
            match rx.recv() {
                Some(Ok(_)) => got += 1,
                Some(Err(status)) => break status,
                None => panic!("the given-up record must be reported"),
            }
        };
        assert_eq!(got, fill);
        assert!(
            status
                .message()
                .unwrap_or_default()
                .contains("1 queued item"),
            "{status:?}"
        );
        runtime.shutdown_timeout(Duration::from_secs(5));
    }

    #[test]
    fn an_end_status_round_trips_through_the_record() {
        for status in [
            Status::from(ExceptionCode::None),
            Status::from((ExceptionCode::Security, "denied")),
            Status::new_service_specific_error(9, Some("busy".to_string())),
            Status::new_service_specific_error(9, None),
        ] {
            let (exception, service_specific, message) =
                super::super::status_fields(&status).expect("carryable");
            let payload = end_payload(exception, service_specific, message.as_deref());
            assert!(payload.len() + HEADER <= END_RESERVE);
            let back = end_status(&payload);
            assert_eq!(back, status);
            assert_eq!(back.message(), status.message());
        }
    }
}

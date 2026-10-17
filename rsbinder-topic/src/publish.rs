// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The publisher's typed side: what a topic is made with, and values encoded into records.

use std::fmt;
use std::marker::PhantomData;
use std::os::fd::OwnedFd;
use std::sync::atomic::{fence, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rsbinder::fmq::shm::Mapping;
use rsbinder::{CallingContext, Status, StatusCode};

use crate::layout::{self, Stored, MIN_RING};
use crate::link::Links;
use crate::payload::{relabel, Piece};
use crate::ring::{Kind, Refused, Stores, Writer};
use crate::{end, Encoding, Payload, PublisherStats};

/// What a topic is made with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PublisherPolicy {
    /// Bytes of ring, from 4 KiB to 1 GiB, rounded up to a multiple of 8. Default 64 KiB.
    ///
    /// The ring is one memfd the publisher allocates up front and every subscriber maps, so it
    /// is charged once, to the publisher, however many subscribers there are. A record is at most
    /// half the ring: the payload of one is at most `ring_bytes / 2 - 8` bytes, 32760 in the
    /// default ring. How many records a subscriber can fall behind by before it loses some is the
    /// ring's size over the size of a record; [`for_payload`](Self::for_payload) works the size
    /// out from that.
    pub ring_bytes: usize,
    /// Subscriptions the topic holds at once. Default 64.
    ///
    /// The publisher maps each subscriber's control page and, after every commit, loads a word of
    /// it, so the bound limits both the mappings a subscriber can make this process hold and the
    /// work of a publish. One more is refused with [`ITopic::ERROR_FULL`]; subscriptions that
    /// ended (the subscriber dropped it, or its process died) are taken out first.
    ///
    /// A publish wakes the subscribers it finds waiting before it lets go of the topic's lock,
    /// so the bound is also the longest a publish holds it and the other publishers wait. The
    /// wake grows with the subscribers that wait: one futex wake for all those of
    /// [`Wake::Futex`], whose kernel time grows with each one it wakes, and one `send` for each
    /// of [`Wake::Fd`]. Measured on an x86_64 desktop (plan 15 §8.4), a publish took about
    /// 0.25 µs with no subscriber waiting, and with 64 waiting about 0.17 ms on futex wakes and
    /// 0.28 ms on fd wakes: about 1–3 µs and 4.5 µs for each subscriber that waits. Lower the
    /// bound where a publish must not stall that long, raise it for more subscribers at that
    /// cost.
    ///
    /// [`ITopic::ERROR_FULL`]: crate::aidl::ITopic::ERROR_FULL
    /// [`Wake::Futex`]: crate::Wake::Futex
    /// [`Wake::Fd`]: crate::Wake::Fd
    pub max_subscribers: usize,
    /// Subscriptions one uid holds at once, refused past it as `max_subscribers` is. Default 64,
    /// which with the default `max_subscribers` limits nothing: lower it so that one uid allowed
    /// to subscribe cannot take every place.
    pub max_per_uid: usize,
    /// Who may subscribe. Default `None`: any caller that can reach the topic's binder, which
    /// for a topic registered with the service manager is any caller it lets find the name.
    ///
    /// Checked first for every subscription, from the publisher's own binder
    /// ([`Publisher::as_binder`](crate::Publisher::as_binder)) and from an
    /// [`Acceptor`](crate::Acceptor) alike, with the caller of the subscription call; a
    /// refusal is `EX_SECURITY`, which a [`NamedSubscriber`](crate::NamedSubscriber) does not
    /// retry on the same publisher. A subscriber in the publisher's process is seen with the
    /// process's own uid and pid.
    pub authorize: Option<Authorize>,
}

impl Default for PublisherPolicy {
    fn default() -> Self {
        Self {
            ring_bytes: 64 << 10,
            max_subscribers: 64,
            max_per_uid: 64,
            authorize: None,
        }
    }
}

/// A check of the caller that a topic runs before it takes a subscription:
/// [`PublisherPolicy::authorize`].
///
/// ```
/// use rsbinder_topic::{Authorize, PublisherPolicy};
///
/// let policy = PublisherPolicy {
///     authorize: Some(Authorize::new(|caller| caller.uid == 1000)),
///     ..PublisherPolicy::default()
/// };
/// # drop(policy);
/// ```
///
/// Two values are equal when they are clones of one check.
#[derive(Clone)]
pub struct Authorize(Arc<dyn Fn(&CallingContext) -> bool + Send + Sync>);

impl Authorize {
    /// A check that admits the callers for which `admit` returns `true`. It runs on the binder
    /// thread the subscription call arrived on, while the call waits.
    pub fn new(admit: impl Fn(&CallingContext) -> bool + Send + Sync + 'static) -> Self {
        Self(Arc::new(admit))
    }

    /// Whether the check admits `caller`.
    pub fn admits(&self, caller: &CallingContext) -> bool {
        (self.0)(caller)
    }
}

impl PartialEq for Authorize {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for Authorize {}

impl fmt::Debug for Authorize {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Authorize").finish_non_exhaustive()
    }
}

impl PublisherPolicy {
    /// The default policy with a ring that keeps the newest `depth` records of up to `max_payload`
    /// bytes each, including while the next one is being written: `(depth + 1) × (8 +
    /// max_payload rounded up to 8)` bytes, and no less than 4 KiB. A `depth` of zero is taken as
    /// one.
    ///
    /// Smaller records take less of the ring, so more of them are kept. A size above 1 GiB is
    /// refused when the topic is made.
    pub fn for_payload(max_payload: usize, depth: usize) -> Self {
        let record = max_payload.div_ceil(8).saturating_mul(8).saturating_add(8);
        let records = depth.max(1).saturating_add(1);
        Self {
            ring_bytes: records.saturating_mul(record).max(MIN_RING),
            ..Self::default()
        }
    }
}

/// Why a publish wrote nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PublishError {
    /// The value encodes to more bytes than a record of this ring carries.
    TooLarge {
        /// The encoded value's length.
        bytes: usize,
        /// The longest payload a record of this ring carries: half the ring, less 8.
        max: usize,
    },
    /// [`Payload::encode`] failed with this. A parcelable that holds a binder fails with
    /// `BadType`, one that holds a file descriptor with `FdsNotAllowed`.
    Encode(StatusCode),
    /// The topic has been closed.
    Closed,
}

impl fmt::Display for PublishError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge { bytes, max } => write!(
                f,
                "a value of {bytes} bytes is larger than the {max} a record of this topic carries"
            ),
            Self::Encode(code) => write!(f, "the value failed to encode: {code}"),
            Self::Closed => f.write_str("the topic is closed"),
        }
    }
}

impl std::error::Error for PublishError {}

impl From<Refused> for PublishError {
    fn from(refused: Refused) -> Self {
        match refused {
            Refused::TooLarge { bytes, max } => Self::TooLarge { bytes, max },
            Refused::Ended | Refused::Spent => Self::Closed,
        }
    }
}

/// What a publish encodes into, kept for the next one: the allocations of an [`Encoding`].
#[derive(Default)]
struct Scratch {
    bytes: Vec<u8>,
    pieces: Vec<Piece<'static>>,
    /// Per record, where its pieces end and its length.
    records: Vec<(usize, usize)>,
}

/// What the publisher's lock guards: the ring's writer and the subscriptions it serves.
pub(crate) struct State<S> {
    pub(crate) writer: Writer<S>,
    pub(crate) links: Links,
    /// Records published, and their payload bytes, for [`PublisherStats`].
    published: u64,
    bytes: u64,
    /// A test's pause after a commit bumps `seq`, before it loads the subscribers' `state`.
    #[cfg(all(test, not(loom)))]
    pub(crate) after_bump: Option<Box<dyn FnMut() + Send>>,
}

impl<S: Stores> State<S> {
    /// After a commit: bump `seq`, then wake the waiters, one futex wake at most (§5.5, §5.5a).
    fn announce(&mut self) {
        let seq = self.writer.announce();
        #[cfg(all(test, not(loom)))]
        if let Some(pause) = self.after_bump.as_mut() {
            pause();
        }
        // Against the `SeqCst` raise of `WAITING`: this load sees it, or the waiter sees new `seq`.
        fence(Ordering::SeqCst);
        if self.links.wake(seq) {
            self.writer.wake();
        }
    }
}

/// A topic's writer behind a lock, fed with values of `T`. Any thread may publish.
pub(crate) struct TypedWriter<T, S> {
    state: Mutex<State<S>>,
    /// Buffers to encode into, one per thread that publishes at a time.
    scratch: Mutex<Vec<Scratch>>,
    max_payload: usize,
    /// The ring's bytes, the most a buffer keeps when it goes back.
    capacity: usize,
    _payload: PhantomData<fn(&T)>,
}

impl<T: Payload> TypedWriter<T, Stored<Mapping>> {
    /// Make topic memory for `policy`: the fd to hand to subscribers, and the writer.
    pub(crate) fn create(policy: &PublisherPolicy) -> rsbinder::fmq::Result<(OwnedFd, Self)> {
        let (fd, stored) = layout::create_topic(policy.ring_bytes)?;
        Ok((fd, Self::new(Writer::new(stored))))
    }
}

impl<T: Payload, S: Stores> TypedWriter<T, S> {
    pub(crate) fn new(writer: Writer<S>) -> Self {
        Self {
            max_payload: writer.max_payload(),
            capacity: usize::try_from(writer.capacity()).unwrap_or(usize::MAX),
            state: Mutex::new(State {
                writer,
                links: Links::default(),
                published: 0,
                bytes: 0,
                #[cfg(all(test, not(loom)))]
                after_bump: None,
            }),
            scratch: Mutex::new(Vec::new()),
            _payload: PhantomData,
        }
    }

    /// What has been published, and the subscriptions held after taking out those that ended.
    pub(crate) fn stats(&self) -> PublisherStats {
        let mut state = self.lock();
        let gone = state.links.prune();
        let stats = PublisherStats {
            published: state.published,
            bytes: state.bytes,
            subscribers: state.links.infos(state.writer.position()),
        };
        drop(state);
        drop(gone);
        stats
    }

    /// The lock every change to the ring and to the list of subscriptions takes.
    pub(crate) fn lock(&self) -> MutexGuard<'_, State<S>> {
        self.state.lock().expect("lock poisoned")
    }

    /// The futex wakes this topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn wakes(&self) -> u64 {
        self.lock().writer.wakes()
    }

    /// The wake bytes this topic has sent.
    #[cfg(all(test, not(loom), any(target_os = "linux", target_os = "android")))]
    pub(crate) fn sends(&self) -> u64 {
        self.lock().links.sends()
    }

    /// Write `value` as one record.
    pub(crate) fn publish(&self, value: &T) -> Result<(), PublishError> {
        self.publish_all([value]).map(drop)
    }

    /// Write each value as a record, all of them or, on any error, none; the count written.
    ///
    /// Every value is encoded before the lock is taken: no code of the application runs under it,
    /// so a panic in `encode` leaves the lock as it was, and an `encode` that publishes to this
    /// topic does not wait for itself.
    pub(crate) fn publish_all<'a, I>(&self, values: I) -> Result<usize, PublishError>
    where
        T: 'a,
        I: IntoIterator<Item = &'a T>,
    {
        let mut scratch = self
            .scratch
            .lock()
            .expect("lock poisoned")
            .pop()
            .unwrap_or_default();
        let mut records = std::mem::take(&mut scratch.records);
        records.clear();
        let mut encoding = Encoding::new(
            std::mem::take(&mut scratch.bytes),
            std::mem::take(&mut scratch.pieces),
        );
        let written = self
            .encode(values, &mut encoding, &mut records)
            .and_then(|()| self.write(&encoding, &records));
        let keeps = |len: usize, most: usize| len <= most;
        if keeps(encoding.bytes.capacity(), self.capacity)
            && keeps(encoding.pieces.capacity(), self.capacity / 8)
            && keeps(records.capacity(), self.capacity / 8)
        {
            scratch.bytes = encoding.bytes;
            scratch.pieces = relabel(encoding.pieces);
            scratch.records = records;
            self.scratch.lock().expect("lock poisoned").push(scratch);
        }
        written
    }

    /// Write the end record with `status`. Later publishes are [`PublishError::Closed`]; a second
    /// close writes nothing.
    pub(crate) fn close(&self, status: &Status) {
        let mut state = self.lock();
        // A 4 KiB ring takes the longest end record, so only an end already written refuses it.
        if state.writer.put(Kind::End, &end::payload(status)).is_ok() {
            state.announce();
        }
    }

    fn encode<'a, I>(
        &self,
        values: I,
        encoding: &mut Encoding<'a>,
        records: &mut Vec<(usize, usize)>,
    ) -> Result<(), PublishError>
    where
        T: 'a,
        I: IntoIterator<Item = &'a T>,
    {
        for value in values {
            encoding.begin();
            value.encode(encoding).map_err(PublishError::Encode)?;
            let bytes = encoding.len;
            if bytes > self.max_payload {
                let max = self.max_payload;
                return Err(PublishError::TooLarge { bytes, max });
            }
            records.push((encoding.pieces.len(), bytes));
        }
        Ok(())
    }

    fn write(
        &self,
        encoding: &Encoding<'_>,
        records: &[(usize, usize)],
    ) -> Result<usize, PublishError> {
        if records.is_empty() {
            return Ok(0);
        }
        let mut state = self.lock();
        // Checked first, so that a refusal comes before the first record and none is written.
        state.writer.admits(records.iter().map(|&(_, len)| len))?;
        let mut first = 0;
        for &(end, len) in records {
            let parts = encoding.pieces[first..end]
                .iter()
                .map(|&piece| encoding.of(piece));
            state.writer.put_parts(Kind::Item, len, parts)?;
            first = end;
        }
        state.published += records.len() as u64;
        state.bytes += records.iter().map(|&(_, len)| len as u64).sum::<u64>();
        state.announce();
        let gone = state.links.prune();
        drop(state);
        drop(gone);
        Ok(records.len())
    }
}

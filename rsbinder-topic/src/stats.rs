// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What each end of a topic can tell about it (plan 15 §5.9a): values read when asked, with no
//! notice of a change.

/// What a [`Publisher`](crate::Publisher) has published and who receives it:
/// [`Publisher::stats`](crate::Publisher::stats).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct PublisherStats {
    /// Records published, the end record left out.
    pub published: u64,
    /// Bytes of payload in them.
    pub bytes: u64,
    /// The subscriptions the topic holds, after taking out those that ended.
    pub subscribers: Vec<SubscriberInfo>,
    /// Subscriptions a wait for room took out
    /// ([`PublisherPolicy::backpressure`](crate::PublisherPolicy::backpressure)), each logged too.
    pub evicted: u64,
    /// Whether the publisher holds a name now: one that
    /// [`advertise`](crate::Publisher::advertise),
    /// [`advertise_with`](crate::Publisher::advertise_with) or
    /// [`advertise_on`](crate::Publisher::advertise_on) gave, which
    /// [`close`](crate::Publisher::close) takes back.
    ///
    /// It says what the publisher holds, not what the resolver keeps: a registry whose host died
    /// lost the name with it, and this stays `true`. Giving the name again through a resolver of
    /// the new host is [`advertise_on`](crate::Publisher::advertise_on) (plan 15 D8).
    pub advertised: bool,
}

/// One subscription as its publisher sees it.
///
/// `lag_bytes`, `lost` and `waiting` come from the subscriber's control page, which the
/// subscriber writes: they say what it reports, and a subscriber that does not follow the
/// protocol can make them say anything. The read position behind `lag_bytes` decides which slot
/// of a pool is lent next and, with backpressure, how long a publish waits: a subscriber that
/// reports it wrongly loses records of its own if ahead, and is taken out after the timeout if
/// behind.
#[derive(Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscriberInfo {
    /// The uid of the process that made the subscription call.
    pub uid: u32,
    /// Its pid; within the publisher's process, this process's.
    pub pid: i32,
    /// Bytes of ring between the subscriber's read position and the publisher's write position.
    pub lag_bytes: u64,
    /// Records the subscriber was told it lost.
    pub lost: u64,
    /// Whether it is about to sleep, or sleeps, for the next record.
    pub waiting: bool,
    /// Whether a wait for room took it out. It holds its place until the subscriber drops it,
    /// and the slot it pins stays pinned.
    pub evicted: bool,
    /// The slot of the topic's pool the subscription pins now, while it holds a
    /// [`Sample`](crate::Sample) of a lent value: the publisher lends no pinned slot. `None`
    /// when it pins none, and on a topic without a pool.
    ///
    /// It is what the subscriber's pin word reads at this call, so it says what the subscriber
    /// reports, as the fields above do, and a pin it takes or lets go of a moment later is not
    /// seen. A subscription that ended is no longer listed, and its pin no longer keeps a slot.
    pub pinned: Option<u32>,
}

/// What a subscriber has received: [`Subscriber::stats`](crate::Subscriber::stats) and
/// [`NamedSubscriber::stats`](crate::NamedSubscriber::stats). Counted by the one subscriber
/// object since it was made, in this process: a subscriber made again starts from zero, and two
/// subscribers of one topic in one process count apart.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct SubscriberStats {
    /// Values returned.
    pub received: u64,
    /// Records reported lost, as [`RecvError::Lagged`](crate::RecvError::Lagged) counts them.
    pub lost: u64,
    /// Records that failed to decode, each one a [`RecvError::Decode`](crate::RecvError::Decode).
    pub decode_errors: u64,
    /// Connections made: 1 for a `Subscriber`; for a `NamedSubscriber`, one per connection it
    /// has received from, the current one included.
    pub connections: u64,
}

impl std::ops::AddAssign for SubscriberStats {
    fn add_assign(&mut self, other: Self) {
        self.received += other.received;
        self.lost += other.lost;
        self.decode_errors += other.decode_errors;
        self.connections += other.connections;
    }
}

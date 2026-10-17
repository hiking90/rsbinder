// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What each end of a topic can tell about it (plan 15 §5.9a): values read when asked, with no
//! notice of a change.

/// What a [`Publisher`](crate::Publisher) has published and who receives it:
/// [`Publisher::stats`](crate::Publisher::stats).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PublisherStats {
    /// Records published, the end record left out.
    pub published: u64,
    /// Bytes of payload in them.
    pub bytes: u64,
    /// The subscriptions the topic holds, after taking out those that ended.
    pub subscribers: Vec<SubscriberInfo>,
}

/// One subscription as its publisher sees it.
///
/// `lag_bytes`, `lost` and `waiting` come from the subscriber's control page, which the
/// subscriber writes: they say what it reports, and a subscriber that does not follow the
/// protocol can make them say anything. Nothing the publisher does depends on them.
#[derive(Clone, Debug, PartialEq, Eq)]
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
}

/// What a subscriber has received: [`Subscriber::stats`](crate::Subscriber::stats) and
/// [`NamedSubscriber::stats`](crate::NamedSubscriber::stats). Counted in this process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
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

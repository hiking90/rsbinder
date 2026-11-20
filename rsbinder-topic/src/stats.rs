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
    /// ([`lossless`](crate::PublisherBuilder::lossless)), each logged too.
    pub evicted: u64,
    /// Whether the publisher holds a name now: one that
    /// [`advertise_on`](crate::Publisher::advertise_on) gave, which
    /// [`close`](crate::Publisher::close) takes back.
    ///
    /// It says what the publisher holds, not what the resolver keeps: a registry whose host died
    /// lost the name with it, and this stays `true`. Giving the name again through a resolver of
    /// the new host is [`advertise_on`](crate::Publisher::advertise_on) (plan 15 D8).
    pub advertised: bool,
}

/// A publisher's list of subscriptions at one moment: [`Publisher::subscribers`] and
/// [`Publisher::wait_subscribers`].
///
/// [`Publisher::subscribers`]: crate::Publisher::subscribers
/// [`Publisher::wait_subscribers`]: crate::Publisher::wait_subscribers
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct Subscribers {
    /// Grows by one with every subscription the list takes in or lets go of, and with nothing
    /// else. Two values that differ mean the list changed in between; how much it grew says
    /// nothing more, since a check that finds several subscriptions ended counts once.
    pub generation: u64,
    /// The subscriptions held, as [`Publisher::subscriber_count`] counts them: those a wait for
    /// room took out are counted until their subscribers drop them.
    ///
    /// [`Publisher::subscriber_count`]: crate::Publisher::subscriber_count
    pub count: usize,
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
    /// The publisher's number for the subscription, from 1 and never given twice by one
    /// publisher: two subscriptions of one process differ in it. The subscriber reads the same
    /// number from [`Subscriber::id`](crate::Subscriber::id).
    pub id: u64,
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
    /// Whether the subscriber asked not to be waited for
    /// ([`SubscriberBuilder::unwaited`](crate::SubscriberBuilder::unwaited)): no publish waits for
    /// it and no wait for room takes it out.
    pub unwaited: bool,
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
    /// Records reported lost, as [`RecvError::Lagged`](crate::RecvError::Lagged) counts them. Stays
    /// 0 on a [`TopicKind::State`](crate::TopicKind::State) topic, whose receives skip past values
    /// without reporting them.
    pub lost: u64,
    /// Records that failed to decode, each one a [`RecvError::Decode`](crate::RecvError::Decode).
    pub decode_errors: u64,
    /// Connections made: 1 for a `Subscriber`; for a `NamedSubscriber`, one per connection it
    /// has received from, the current one included.
    pub connections: u64,
}

/// The text of a topic's binder dump: one line for the topic, then one per subscription, each
/// ending in a newline. `rsb_service topic stat` prints the same.
impl std::fmt::Display for crate::aidl::TopicStatus::TopicStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let kind = match crate::TopicKind::from_wire(self.topicKind) {
            Some(crate::TopicKind::State) => "state",
            Some(_) => "event",
            None => "unknown",
        };
        let waits = if self.lossless { "lossless" } else { "lossy" };
        // The `long`s carry `u64` bits, the uids `u32` ones.
        writeln!(
            f,
            "{} {kind} topic: ring {} bytes, {waits}, published {} ({} bytes), evicted {}, {} \
             subscribers",
            self.typeName,
            self.ringBytes as u64,
            self.published as u64,
            self.bytes as u64,
            self.evicted as u64,
            self.subscribers.len()
        )?;
        for one in &self.subscribers {
            write!(
                f,
                "  #{} uid {} pid {}: lag {} bytes, lost {}",
                one.id as u64, one.uid as u32, one.pid, one.lagBytes as u64, one.lost as u64
            )?;
            let marks = [
                (one.waiting, ", waiting"),
                (one.evicted, ", evicted"),
                (one.unwaited, ", unwaited"),
            ];
            for (_, mark) in marks.iter().filter(|(set, _)| *set) {
                f.write_str(mark)?;
            }
            if one.pinned >= 0 {
                write!(f, ", pins slot {}", one.pinned)?;
            }
            writeln!(f)?;
        }
        Ok(())
    }
}

impl std::ops::AddAssign for SubscriberStats {
    fn add_assign(&mut self, other: Self) {
        self.received += other.received;
        self.lost += other.lost;
        self.decode_errors += other.decode_errors;
        self.connections += other.connections;
    }
}

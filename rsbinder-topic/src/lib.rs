// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One publisher and many subscribers over kernel binder (plan 15). **Experimental.**
//!
//! A topic carries records from one publisher to any number of subscribers. A binder call sets
//! a subscription up; after it the records travel through shared memory and no binder
//! transaction is made for them:
//!
//! * The publisher allocates the topic's memory, a ring of records in a sealed memfd, and
//!   every subscriber maps it read-only. One copy into the ring serves all of them.
//! * A publish does not wait for a subscriber. When the ring is full the oldest records are
//!   overwritten, and a subscriber that had not read them is told how many it lost. A topic
//!   with [`Backpressure`] waits instead, up to a timeout, for its subscribers to read what a
//!   publish would overwrite, and then takes out the ones still in the way
//!   ([`Closed::Evicted`]). A receiver that must see every value however slow it is uses a
//!   stream (`rsbinder::stream`), which holds the sender back and takes no receiver out.
//! * A subscriber writes one small memfd of its own, its control page, which the publisher
//!   maps read-only. Nothing a subscriber does reaches the ring or another subscriber.
//!
//! A topic carries values of one type, a [`Payload`]: any AIDL parcelable, or [`Bytes`]. Each
//! publish encodes one value into one record, and each receive decodes one record or says why
//! it returned none ([`RecvError`]): nothing was unread, records were lost, one failed to decode,
//! or the topic ended.
//!
//! A value too large to copy twice goes through a pool instead ([`PoolPolicy`]): the publisher
//! writes it straight into a slot of a second memfd ([`Publisher::loan_slice`], [`LoanMut`]) and
//! the ring carries only a record that names the slot. A [`Bytes`] subscriber receives it as any
//! other value, or where it lies with [`Subscriber::recv_sample`]: the [`Sample`] pins the slot,
//! and the publisher lends no pinned slot. A subscriber copies a lent value out with atomic loads
//! unless it vouches for its publisher ([`Subscriber::trust_publisher`], [`Trusted`]); then its
//! samples deref to `[u8]` in place and its copies are plain memory copies.
//!
//! A receive may wait for a record ([`Subscriber::recv`], `recv_timeout`, `recv_latest`, and
//! `recv_async` with the `tokio` feature). A waiting subscriber sleeps on a futex word in the
//! topic's memory; a publish wakes every subscriber that sleeps with one system call, and makes
//! none when none sleeps. An [`Interrupter`] ends a wait from another thread. A subscriber made
//! with [`Wake::Fd`] is woken through a socket of its own instead, a system call per waiting
//! subscriber, and has file descriptors to poll: one thread waits for several subscriptions with
//! [`wait_any`] or an event loop ([`Wait`]), and `recv_async` holds no thread while it waits.
//!
//! A [`Publisher`] makes the topic and hands out an `ITopic` binder; a [`Subscriber`] takes a
//! subscription through it with one twoway call. Each end learns of the other's end: a
//! subscriber that is dropped marks its control page, one whose process dies is reported to the
//! publisher through binder, and a publisher that is dropped or closed writes an end record,
//! while one whose process dies is reported to its subscribers.
//!
//! ```no_run
//! use rsbinder::{hub, ProcessState};
//! use rsbinder_topic::{Bytes, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! ProcessState::init_default()?;
//! ProcessState::start_thread_pool();
//!
//! // The publishing process.
//! let publisher = Publisher::<Bytes>::new(&PublisherPolicy::default())?;
//! hub::add_service("example.topic", publisher.as_binder())?;
//! publisher.publish(&Bytes(b"hello".to_vec()))?;
//!
//! // A subscribing process.
//! let topic = hub::check_service("example.topic").ok_or("no topic")?;
//! let mut subscriber = Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default())?;
//! match subscriber.recv() {
//!     Ok(Bytes(value)) => println!("{value:?}"),
//!     Err(why) => println!("{why}"),
//! }
//! # Ok(()) }
//! ```
//!
//! A topic can also be found by name, the publisher and the subscriber starting in either
//! order: [`Publisher::advertise`] registers the binder with the service manager, and a
//! [`NamedSubscriber`] watches the name and connects to whichever publisher holds it, again and
//! again. Where a `Subscriber`'s topic ends with [`RecvError::Closed`], a `NamedSubscriber`
//! returns [`RecvError::Disconnected`] once and waits for the next publisher, except after
//! [`Closed::Evicted`], which ends it. Names come from a
//! [`Resolver`], the service manager by default ([`ServiceManager`]). Who may subscribe is
//! [`PublisherPolicy::authorize`].
//!
//! The subscription call never leaves a process whose publisher is in it, so a subscriber of a
//! publisher of its own process needs no binder device. An RPC binder is not subscribed to.
//!
//! Topics need Linux or Android: a kernel with `F_SEAL_FUTURE_WRITE` (mainline 5.1, the
//! Android common kernels from 4.14) and a target on which a 64-bit word of a read-only
//! mapping can be loaded (`rsbinder::fmq::shm::RoWord64::PRESENT`). Elsewhere the crate
//! compiles and every constructor returns `Unsupported`.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]

pub mod aidl;
mod backpressure;
mod connect;
mod end;
mod layout;
mod link;
mod named;
mod payload;
mod poll;
mod pool;
mod publish;
mod publisher;
mod receive;
mod resolve;
mod ring;
mod sample;
mod stats;
mod subscriber;
mod trust;
mod wait;
mod wake;

pub use backpressure::Backpressure;
pub use named::NamedSubscriber;
pub use payload::{Bytes, Encoding, Payload, Record};
pub use poll::{wait_any, Wait};
pub use publish::{Authorize, PoolPolicy, PublishError, PublisherPolicy};
pub use publisher::{Acceptor, LoanMut, Publisher};
pub use receive::{Closed, Disconnected, RecvError};
pub use resolve::{Resolver, ServiceManager, Watch};
pub use sample::Sample;
pub use stats::{PublisherStats, SubscriberInfo, SubscriberStats};
pub use subscriber::{Interrupter, Start, Subscriber, SubscriberPolicy, Wake};
pub use trust::{Trust, Trusted, Untrusted};

#[cfg(any(test, feature = "fuzzing"))]
mod image;

#[cfg(all(
    test,
    not(loom),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod typed_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod connect_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod loan_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod backpressure_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod named_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod wait_tests;

#[cfg(all(
    test,
    not(loom),
    not(miri),
    any(target_os = "linux", target_os = "android"),
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        all(target_arch = "arm", target_has_atomic = "64")
    )
))]
mod fd_wait_tests;

#[cfg(all(test, not(any(target_os = "linux", target_os = "android"))))]
mod unsupported_tests {
    use rsbinder::{ExceptionCode, Interface};

    use crate::{Bytes, Publisher, PublisherPolicy, Subscriber, SubscriberPolicy};

    #[test]
    fn neither_end_of_a_topic_is_made_on_a_target_without_memfd() {
        let made = Publisher::<Bytes>::new(&PublisherPolicy::default());
        assert_eq!(made.err(), Some(rsbinder::fmq::Error::Unsupported));
        let topic = rsbinder::Binder::new(crate::connect::Token).as_binder();
        let refused = Subscriber::<Bytes>::request(&topic, &SubscriberPolicy::default());
        assert_eq!(
            refused.err().map(|status| status.exception_code()),
            Some(ExceptionCode::UnsupportedOperation)
        );
    }
}

/// Entry points for the `fuzz/` crate; not part of the API.
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing {
    /// Read `image` as topic memory a hostile publisher wrote: 64 bytes of header, then the ring.
    ///
    /// Panics when a receive call does more work than its bound allows; a read outside the
    /// image is the sanitizer's to report.
    pub fn recv_image(image: &[u8]) {
        let _ = crate::image::drive(image);
    }
}

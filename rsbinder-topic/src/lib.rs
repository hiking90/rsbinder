// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One publisher and many subscribers over kernel binder (plan 15). **Experimental, and not
//! complete: this version has no publisher or subscriber yet, only the types they will take and
//! return.**
//!
//! A topic carries records from one publisher to any number of subscribers. A binder call sets
//! a subscription up; after it the records travel through shared memory and no binder
//! transaction is made for them:
//!
//! * The publisher allocates the topic's memory, a ring of records in a sealed memfd, and
//!   every subscriber maps it read-only. One copy into the ring serves all of them.
//! * A publish never waits. When the ring is full the oldest records are overwritten, and a
//!   subscriber that had not read them is told how many it lost. A receiver that must see
//!   every value uses a stream (`rsbinder::stream`) instead, which holds the sender back.
//! * A subscriber writes one small memfd of its own, its control page, which the publisher
//!   maps read-only. Nothing a subscriber does reaches the ring or another subscriber.
//!
//! A topic carries values of one type, a [`Payload`]: any AIDL parcelable, or [`Bytes`]. Each
//! publish encodes one value into one record, and each receive decodes one record or says why
//! it returned none ([`RecvError`]): nothing was unread, records were lost, one failed to decode,
//! or the topic ended.
//!
//! What is here so far is the part that needs no binder: these types, the layout of the two
//! memory objects, the record format, and the algorithms that write the ring and read it while it
//! is being overwritten. The subscription call and the waiting receive come next.
//!
//! Topics need Linux or Android: a kernel with `F_SEAL_FUTURE_WRITE` (mainline 5.1, the
//! Android common kernels from 4.14) and a target on which a 64-bit word of a read-only
//! mapping can be loaded (`rsbinder::fmq::shm::RoWord64::PRESENT`). Elsewhere the crate
//! compiles and every constructor returns `Unsupported`.

#![cfg_attr(not(test), deny(clippy::unwrap_used))]
#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]
// Not in the Linux test build, whose tests call all of it; the others leave the mapped ones out.
#![cfg_attr(
    any(
        not(test),
        loom,
        miri,
        not(any(target_os = "linux", target_os = "android"))
    ),
    expect(
        dead_code,
        reason = "the shared-memory core has no caller until the subscription layer exists"
    )
)]

mod end;
mod layout;
mod payload;
mod publish;
mod receive;
mod ring;

pub use payload::{Bytes, Payload};
pub use publish::{PublishError, PublisherPolicy};
pub use receive::{Closed, RecvError};

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

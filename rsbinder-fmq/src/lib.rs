// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AOSP-compatible Fast Message Queue (FMQ) in Rust.
//!
//! An FMQ is a ring of fixed-size elements in shared memory with two
//! counters and, optionally, a 32-bit futex word, as Android's `libfmq`
//! (`system/libfmq`) lays it out. AIDL HALs hand one across binder as an
//! `android.hardware.common.fmq.MQDescriptor`; this crate is the part that
//! does not depend on binder — making the memory, describing it, attaching
//! to a peer's description, and moving elements through it. `rsbinder`
//! carries the `MQDescriptor` parcelable and the conversion to
//! [`Descriptor`].
//!
//! # Compatibility scope
//!
//! * **Flavors**: both of libfmq's, each with types of its own.
//!   `SynchronizedReadWrite` — one reader, one writer, a full ring refuses
//!   the write — is [`MessageQueue`]. Keeping to one reader and one writer
//!   is the callers' protocol, as in libfmq; nothing detects a second one.
//!   `UnsynchronizedWrite` — one writer, any number of readers, a full ring
//!   overwrites its oldest elements and a reader that had not read them is
//!   told so — is [`UnsyncWriter`] and [`UnsyncReader`], with the counter
//!   discipline of `android-17.0.0_r1`: the write region end
//!   (`android-16.0.0_r4`), the resume half a ring behind the writer
//!   (`android-15.0.0_r6`) and `readLatest`. [`UnsyncWriter`] and
//!   [`UnsyncReader`] say what holds against a libfmq peer older than
//!   those.
//! * **Layout**: grantor 0 = read counter (`u64`, bytes consumed), 1 = write
//!   counter (`u64`, bytes produced), 2 = the ring, 3 = the EventFlag word
//!   (`u32`, optional). Offsets are multiples of 8. Counters never wrap; a
//!   position is `counter % ring bytes`. Unchanged in libfmq from Android 12
//!   through 17. In the unsynchronized flavor every reader keeps its read
//!   counter in its own process, and grantor 0 is the writer's *write region
//!   end*: the counter value the write in progress runs up to.
//! * **EventFlag**: `FUTEX_WAIT_BITSET` / `FUTEX_WAKE_BITSET` on the shared
//!   word without `FUTEX_PRIVATE_FLAG`, bits [`NOT_FULL`] and [`NOT_EMPTY`]
//!   as libfmq names them, a wait consuming the bits in its mask. See
//!   [`EventFlag`]. The unsynchronized flavor has no blocking operations, in
//!   libfmq or here.
//! * **Elements**: primitive integers and floats ([`Element`]). AIDL
//!   `@FixedSize` parcelables need a `repr(C)` type with the AIDL's
//!   layout, which the AIDL compiler is the place to generate.
//! * **Memory**: a queue this crate creates is a memfd sealed against
//!   resizing, with every page allocated up front. One made by libfmq is
//!   ashmem (or, on Android 11+, a memfd sealed `GROW | SHRINK` by
//!   libcutils); both attach. An unsynchronized queue can be sealed
//!   `F_SEAL_FUTURE_WRITE` as well
//!   ([`UnsyncCreate::read_only_readers`](field@UnsyncCreate::read_only_readers)),
//!   so that only its writer has it mapped writable.
//!
//! # Shared-memory primitives
//!
//! What the queue is built from is public as the [`shm`] module: a memfd
//! and its seals, a mapping at any byte offset, copies in and out of shared
//! memory through atomics, and non-private futex wait and wake. A protocol
//! other than FMQ can be built on the same module; `rsbinder`'s shared-memory
//! code asks it about a peer's fd ([`shm::is_ashmem_fd`],
//! [`shm::ashmem_size`]). The module carries no layout and no protocol; those
//! are the queue's.
//!
//! # Platforms
//!
//! The queue works on Linux and Android targets that have 64-bit atomics.
//! The crate also compiles for macOS, for a Linux target without 64-bit
//! atomics (`armv5te-unknown-linux-gnueabi`) and for WASI
//! (`wasm32-wasip1`); there [`MessageQueue::create`],
//! [`MessageQueue::attach`], [`UnsyncWriter::create`] and
//! [`UnsyncReader::attach`] return [`Error::Unsupported`]. The [`shm`]
//! module says which of its items each of those targets has.
//! [`UnsyncReader`] reads 64-bit counters through a read-only mapping and so
//! needs `shm::RoWord64` too; a 32-bit x86 target without SSE2 has 64-bit
//! atomics and no such type, and `UnsyncReader::attach` is `Unsupported`
//! there.
//!
//! # A descriptor is untrusted
//!
//! The descriptor comes from the peer, and the memory it names is the
//! peer's to rewrite at any time. [`MessageQueue::attach`] therefore takes
//! an [`AttachPolicy`] naming what the receiver will accept (largest ring,
//! seals, the EventFlag word), checks every field libfmq checks and the
//! sizes and seals it does not, and every operation re-checks the counters
//! before trusting them. What is copied out of the ring is the only thing a
//! reader interprets.
//!
//! Every access this crate makes to the shared memory — counters, ring and
//! EventFlag word — is a Rust atomic, so a change made from outside (the
//! peer, another handle on the same memory, C code, `rsbinder`'s
//! `SharedMemory` mapping the same fd) is another thread's store in Rust's
//! memory model. Breaking the protocol that way gives [`Error::Corrupted`]
//! or wrong elements, not undefined behavior; nothing detects it. See
//! "Soundness" on [`Regions`].

#![warn(missing_docs)]
// Library code returns errors; tests may unwrap (plan 13).
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
// Every allow outside tests says why (plan 13-1).
#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]

mod descriptor;
mod error;
mod event_flag;
mod queue;
mod ring;
pub mod shm;
mod unsync;

pub use descriptor::{AttachPolicy, Descriptor, Flavor, Grantor};
pub use error::{Error, Result};
pub use event_flag::{EventFlag, NOT_EMPTY, NOT_FULL};
pub use queue::{Element, MessageQueue, Regions};
pub use unsync::{UnsyncCreate, UnsyncRead, UnsyncReader, UnsyncStart, UnsyncWriter};

#[cfg(all(
    test,
    not(loom),
    any(target_os = "linux", target_os = "android"),
    target_has_atomic = "64"
))]
mod reader_target_tests {
    use super::*;

    /// `tests/unsync.rs` runs only where a reader exists; this states what the other targets get.
    #[test]
    fn a_reader_attaches_exactly_where_the_wide_word_exists() {
        let writer = UnsyncWriter::<u8>::create(8, &UnsyncCreate::new()).expect("create");
        let desc = writer.descriptor().expect("descriptor");
        let policy = AttachPolicy {
            max_capacity: 8,
            require_seal: true,
            require_event_flag: false,
        };
        let attached = UnsyncReader::<u8>::attach(&desc, &policy, UnsyncStart::Oldest);
        if shm::RoWord64::PRESENT {
            assert!(attached.is_ok(), "{:?}", attached.err());
        } else {
            assert_eq!(attached.err(), Some(Error::Unsupported));
        }
    }
}

#[cfg(all(test, not(any(target_os = "linux", target_os = "android"))))]
mod unsupported_tests {
    use super::*;

    #[test]
    fn create_reports_unsupported() {
        assert_eq!(
            MessageQueue::<u8>::create(16, true).err(),
            Some(Error::Unsupported)
        );
    }

    #[test]
    fn the_target_verdict_comes_before_the_arguments() {
        // On a target that runs a queue a capacity of zero is `BadValue`.
        assert_eq!(
            MessageQueue::<u8>::create(0, true).err(),
            Some(Error::Unsupported)
        );
        let desc = Descriptor {
            fds: vec![std::fs::File::open("/dev/null").expect("/dev/null").into()],
            ints: Vec::new(),
            grantors: Vec::new(),
            quantum: 1,
            flavor: Flavor::SynchronizedReadWrite,
        };
        let policy = AttachPolicy {
            max_capacity: 1,
            require_seal: true,
            require_event_flag: true,
        };
        assert_eq!(
            MessageQueue::<u8>::attach(&desc, &policy).err(),
            Some(Error::Unsupported)
        );
        assert_eq!(
            UnsyncWriter::<u8>::create(0, &UnsyncCreate::new()).err(),
            Some(Error::Unsupported)
        );
        assert_eq!(
            UnsyncReader::<u8>::attach(&desc, &policy, UnsyncStart::Now).err(),
            Some(Error::Unsupported)
        );
    }

    #[test]
    fn memfd_seals_and_futex_report_unsupported() {
        use std::os::fd::AsFd;
        use std::sync::atomic::AtomicU32;
        let options = shm::CreateOptions::new("unit");
        assert_eq!(shm::create(4096, &options).err(), Some(Error::Unsupported));
        let file = std::fs::File::open("/dev/null").expect("/dev/null");
        let seals = shm::Seals::GROW;
        assert_eq!(shm::add_seals(file.as_fd(), seals), Err(Error::Unsupported));
        assert_eq!(shm::seals(file.as_fd()), Err(Error::Unsupported));
        assert!(!shm::shrink_sealed(file.as_fd()));
        let word = AtomicU32::new(0);
        assert_eq!(shm::futex_wait(&word, 0, 1, None), Err(Error::Unsupported));
        assert_eq!(shm::futex_wake(&word, 1), Err(Error::Unsupported));
    }
}

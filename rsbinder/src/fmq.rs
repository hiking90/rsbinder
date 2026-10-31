// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fast Message Queue: the AIDL `MQDescriptor` a queue is handed across
//! binder as, and the `rsbinder-fmq` crate that makes and attaches the
//! queue itself.
//!
//! An FMQ (`system/libfmq`) is a ring of fixed-size elements in shared
//! memory. A HAL creates one, describes it as
//! `android.hardware.common.fmq.MQDescriptor<T, Flavor>` and returns that
//! parcelable from an AIDL method; the peer attaches to the memory it names.
//! The parts split the way AOSP splits them:
//!
//! * The parcelables — [`MQDescriptor`], [`GrantorDescriptor`],
//!   [`SynchronizedReadWrite`], [`UnsynchronizedWrite`] and
//!   [`NativeHandle`](crate::NativeHandle) — are generated from the AOSP
//!   `.aidl` (`hardware/interfaces/common`, VINTF-stable) and are the wire
//!   format. They exist on every platform. An `.aidl` that imports these
//!   packages compiles against them without a vendored copy: `rsbinder-aidl`
//!   names `rsbinder::fmq::MQDescriptor` and `rsbinder::NativeHandle` for
//!   those imports.
//! * The queue — `MessageQueue`, `EventFlag`, `Descriptor`, `AttachPolicy`,
//!   and for the unsynchronized flavor `UnsyncWriter` and `UnsyncReader` —
//!   is `rsbinder-fmq`, re-exported here on every platform. Making or
//!   attaching a queue works on Linux and Android targets with 64-bit
//!   atomics, where the shared memory, futex and counters it needs exist,
//!   and returns `Error::Unsupported` elsewhere. `UnsyncReader::attach`
//!   needs `shm::RoWord64` as well and returns `Unsupported` on a target
//!   without that type (32-bit x86 without SSE2), and `UnsyncWriter::create`
//!   with `read_only_readers` returns it on a kernel without
//!   `F_SEAL_FUTURE_WRITE`.
//! * The two conversions below join them: an [`MQDescriptor`] that arrived in
//!   a parcel becomes a `Descriptor` to attach to (`TryFrom<&MQDescriptor>`),
//!   and a queue's `Descriptor` becomes the [`MQDescriptor`] to send
//!   ([`MQDescriptor::try_from_sized`], which also checks the element size,
//!   or `TryFrom<Descriptor>`).
//!
//! ```no_run
//! # #[cfg(any(target_os = "linux", target_os = "android"))]
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! use rsbinder::fmq::{AttachPolicy, Descriptor, MQDescriptor, MessageQueue, SynchronizedReadWrite};
//!
//! // The service side: make a queue and describe it for the reply.
//! let queue = MessageQueue::<u8>::create(1024, true)?;
//! let reply = MQDescriptor::<i8, SynchronizedReadWrite>::try_from_sized(queue.descriptor()?)?;
//!
//! // The client side: attach to what the reply named.
//! let desc = Descriptor::try_from(&reply)?;
//! let policy = AttachPolicy { max_capacity: 4096, require_seal: true, require_event_flag: true };
//! let reader = MessageQueue::<u8>::attach(&desc, &policy)?;
//! # let _ = reader; Ok(()) }
//! # #[cfg(not(any(target_os = "linux", target_os = "android")))]
//! # fn main() {}
//! ```
//!
//! The AIDL element type and the Rust element type are two names for one
//! size: `MQDescriptor<byte, …>` is `MQDescriptor<i8, …>` and the queue over
//! it is `MessageQueue<u8>` or `MessageQueue<i8>`; what matters is
//! `quantum`, which `MessageQueue::attach` checks against `size_of::<T>()`.
//!
//! `TryFrom<&MQDescriptor>` duplicates the fds, so the parcelable stays
//! usable (for example to forward it). It returns `BadValue` when a signed
//! field is negative or `flags` is not `F`'s flavor; the geometry itself is
//! checked by [`MessageQueue::attach`](rsbinder_fmq::MessageQueue::attach).
//! `TryFrom<Descriptor>` takes the descriptor's fds and returns `BadValue`
//! when the descriptor's flavor is not `F`, or when a value does not fit the
//! AIDL's signed field (an offset past `i32::MAX`, which no queue libfmq or
//! `rsbinder-fmq` makes has).
//!
//! `rsbinder-fmq`'s `Error` becomes a binder `StatusCode` as follows. A
//! descriptor or policy check and a corrupted counter are both `BadValue`
//! (the caller cannot tell a hostile peer from a defective one and treats
//! both as the end of the queue), a missing EventFlag word or an unsupported
//! platform is `InvalidOperation`, a wait that ran out is `TimedOut`, and an
//! OS error keeps its errno.

pub(crate) mod generated {
    include!(concat!(env!("OUT_DIR"), "/fmq.rs"));
}

pub use generated::android::hardware::common::fmq::{
    GrantorDescriptor::GrantorDescriptor, MQDescriptor::MQDescriptor,
    SynchronizedReadWrite::SynchronizedReadWrite, UnsynchronizedWrite::UnsynchronizedWrite,
};
pub use rsbinder_fmq::*;

pub use convert::FlavorType;

mod convert {
    use std::os::fd::OwnedFd;

    // `NativeHandle` is re-exported at the crate root only (`crate::NativeHandle`).
    use super::generated::android::hardware::common::NativeHandle::NativeHandle;
    use super::{GrantorDescriptor, MQDescriptor, SynchronizedReadWrite, UnsynchronizedWrite};
    use crate::{ParcelFileDescriptor, StatusCode};
    use rsbinder_fmq::{Descriptor, Flavor, Grantor};

    mod private {
        pub trait Sealed {}
        impl Sealed for super::SynchronizedReadWrite {}
        impl Sealed for super::UnsynchronizedWrite {}
    }

    /// The AIDL flavor marker of an `MQDescriptor<T, F>` as the queue's
    /// [`Flavor`]: the two conversions below check `flags` against it, so a
    /// parcelable whose type argument and wire flavor disagree is refused
    /// instead of sent.
    pub trait FlavorType: private::Sealed {
        const FLAVOR: Flavor;
    }

    impl FlavorType for SynchronizedReadWrite {
        const FLAVOR: Flavor = Flavor::SynchronizedReadWrite;
    }

    impl FlavorType for UnsynchronizedWrite {
        const FLAVOR: Flavor = Flavor::UnsynchronizedWrite;
    }

    /// `rsbinder-fmq`'s error as a binder status; the mapping is in the module doc.
    impl From<rsbinder_fmq::Error> for StatusCode {
        fn from(e: rsbinder_fmq::Error) -> Self {
            match e {
                rsbinder_fmq::Error::BadValue(_) | rsbinder_fmq::Error::Corrupted(_) => {
                    StatusCode::BadValue
                }
                rsbinder_fmq::Error::NoEventFlag | rsbinder_fmq::Error::Unsupported => {
                    StatusCode::InvalidOperation
                }
                rsbinder_fmq::Error::TimedOut => StatusCode::TimedOut,
                rsbinder_fmq::Error::Os(errno) => StatusCode::from(errno),
                // `Error` is `#[non_exhaustive]`; a new variant is `Unknown` until named here.
                _ => StatusCode::Unknown,
            }
        }
    }

    /// The parcelable as a [`Descriptor`], with duplicated fds; the checks are in the module doc.
    impl<T, F: FlavorType> TryFrom<&MQDescriptor<T, F>> for Descriptor {
        type Error = StatusCode;

        fn try_from(desc: &MQDescriptor<T, F>) -> Result<Self, StatusCode> {
            let flags = u32::try_from(desc.flags).map_err(|_| StatusCode::BadValue)?;
            let flavor = Flavor::from_flags(flags)
                .filter(|flavor| *flavor == F::FLAVOR)
                .ok_or(StatusCode::BadValue)?;
            let quantum = u32::try_from(desc.quantum).map_err(|_| StatusCode::BadValue)?;
            let grantors = desc
                .grantors
                .iter()
                .map(|g| {
                    Ok(Grantor {
                        fd_index: u32::try_from(g.fdIndex).map_err(|_| StatusCode::BadValue)?,
                        offset: u32::try_from(g.offset).map_err(|_| StatusCode::BadValue)?,
                        extent: u64::try_from(g.extent).map_err(|_| StatusCode::BadValue)?,
                    })
                })
                .collect::<Result<Vec<_>, StatusCode>>()?;
            let fds = desc
                .handle
                .fds
                .iter()
                .map(|fd| fd.try_clone().map(OwnedFd::from))
                .collect::<Result<Vec<_>, StatusCode>>()?;
            Ok(Descriptor {
                fds,
                ints: desc.handle.ints.clone(),
                grantors,
                quantum,
                flavor,
            })
        }
    }

    /// The descriptor as the parcelable to send, taking its fds; the checks are in the module doc.
    impl<T, F: FlavorType> TryFrom<Descriptor> for MQDescriptor<T, F> {
        type Error = StatusCode;

        fn try_from(desc: Descriptor) -> Result<Self, StatusCode> {
            if desc.flavor != F::FLAVOR {
                return Err(StatusCode::BadValue);
            }
            let grantors = desc
                .grantors
                .iter()
                .map(|g| {
                    Ok(GrantorDescriptor {
                        fdIndex: i32::try_from(g.fd_index).map_err(|_| StatusCode::BadValue)?,
                        offset: i32::try_from(g.offset).map_err(|_| StatusCode::BadValue)?,
                        extent: i64::try_from(g.extent).map_err(|_| StatusCode::BadValue)?,
                    })
                })
                .collect::<Result<Vec<_>, StatusCode>>()?;
            let quantum = i32::try_from(desc.quantum).map_err(|_| StatusCode::BadValue)?;
            let flags = i32::try_from(desc.flavor.flags()).map_err(|_| StatusCode::BadValue)?;
            let handle = NativeHandle {
                fds: desc
                    .fds
                    .into_iter()
                    .map(ParcelFileDescriptor::from)
                    .collect(),
                ints: desc.ints,
            };
            Ok(MQDescriptor {
                grantors,
                handle,
                quantum,
                flags,
                ..Default::default()
            })
        }
    }

    impl<T, F: FlavorType> MQDescriptor<T, F> {
        /// `desc` as the parcelable to send, as `TryFrom<Descriptor>` makes
        /// it, and `BadValue` also when the queue's `quantum` is not
        /// `size_of::<T>()`.
        ///
        /// `TryFrom<Descriptor>` does not look at `T`, and the AIDL method's
        /// return type infers it, so a queue of `u32` converts to an
        /// `MQDescriptor<i8, _>` without a word; the peer's libfmq then
        /// refuses the descriptor ("Payload size differs") and the failure
        /// shows on the peer only. This refuses it here. For a
        /// `@FixedSize` parcelable `T`, whose Rust layout need not be the
        /// C++ `sizeof`, use `TryFrom<Descriptor>` and size the queue's
        /// element type to the peer's.
        pub fn try_from_sized(desc: Descriptor) -> Result<Self, StatusCode> {
            if usize::try_from(desc.quantum).ok() != Some(std::mem::size_of::<T>()) {
                return Err(StatusCode::BadValue);
            }
            Self::try_from(desc)
        }
    }

    // `MessageQueue::create` is `Unsupported` off Linux/Android.
    #[cfg(all(test, any(target_os = "linux", target_os = "android")))]
    mod tests {
        use super::*;
        use rsbinder_fmq::MessageQueue;

        type Sync = super::super::SynchronizedReadWrite;

        #[test]
        fn a_sized_conversion_refuses_an_element_of_another_size() {
            let queue = MessageQueue::<u32>::create(64, true).unwrap();
            let wrong = MQDescriptor::<i8, Sync>::try_from_sized(queue.descriptor().unwrap());
            assert_eq!(wrong.err(), Some(StatusCode::BadValue));
            let right = MQDescriptor::<i32, Sync>::try_from_sized(queue.descriptor().unwrap());
            assert_eq!(right.unwrap().quantum, 4);
        }

        #[test]
        fn descriptor_round_trips_through_the_parcelable() {
            let queue = MessageQueue::<u32>::create(64, true).unwrap();
            let original = queue.descriptor().unwrap();
            let expected_grantors = original.grantors.clone();
            let expected_ints = original.ints.clone();

            let parcelable: MQDescriptor<i32, Sync> = original.try_into().unwrap();
            assert_eq!(parcelable.quantum, 4);
            assert_eq!(parcelable.flags, 1);
            assert_eq!(parcelable.grantors.len(), 4);
            assert_eq!(parcelable.handle.fds.len(), 1);

            let back = Descriptor::try_from(&parcelable).unwrap();
            assert_eq!(back.grantors, expected_grantors);
            assert_eq!(back.ints, expected_ints);
            assert_eq!(back.quantum, 4);
            assert_eq!(back.flavor, Flavor::SynchronizedReadWrite);
            // The fds are duplicates, not the parcelable's own.
            assert_eq!(back.fds.len(), 1);
            assert_ne!(
                std::os::fd::AsRawFd::as_raw_fd(&back.fds[0]),
                std::os::fd::AsRawFd::as_raw_fd(&parcelable.handle.fds[0])
            );

            // And the duplicate attaches like the original would.
            let policy = rsbinder_fmq::AttachPolicy {
                max_capacity: 64,
                require_seal: true,
                require_event_flag: true,
            };
            let mut reader = MessageQueue::<u32>::attach(&back, &policy).unwrap();
            let mut writer = queue;
            assert!(writer.write(&[7, 8, 9]).unwrap());
            let mut out = [0u32; 3];
            assert!(reader.read(&mut out).unwrap());
            assert_eq!(out, [7, 8, 9]);
        }

        #[test]
        fn a_flavor_that_disagrees_with_the_type_argument_is_bad_value() {
            type Unsync = super::super::UnsynchronizedWrite;
            let queue = MessageQueue::<u32>::create(64, true).unwrap();
            let original = queue.descriptor().unwrap();
            assert_eq!(original.flavor, Flavor::SynchronizedReadWrite);
            assert_eq!(
                MQDescriptor::<i32, Unsync>::try_from(original).unwrap_err(),
                StatusCode::BadValue
            );

            let parcelable = MQDescriptor::<i8, Unsync> {
                flags: 1,
                quantum: 1,
                ..Default::default()
            };
            assert_eq!(
                Descriptor::try_from(&parcelable).unwrap_err(),
                StatusCode::BadValue
            );
        }

        #[test]
        fn negative_fields_are_bad_value() {
            let mut parcelable = MQDescriptor::<i8, Sync> {
                flags: 1,
                quantum: -1,
                ..Default::default()
            };
            assert_eq!(
                Descriptor::try_from(&parcelable).unwrap_err(),
                StatusCode::BadValue
            );
            parcelable.quantum = 1;
            parcelable.flags = 3;
            assert_eq!(
                Descriptor::try_from(&parcelable).unwrap_err(),
                StatusCode::BadValue
            );
            parcelable.flags = 1;
            parcelable.grantors.push(GrantorDescriptor {
                fdIndex: 0,
                offset: -8,
                extent: 8,
            });
            assert_eq!(
                Descriptor::try_from(&parcelable).unwrap_err(),
                StatusCode::BadValue
            );
        }

        #[test]
        fn error_maps_to_status_code() {
            assert_eq!(
                StatusCode::from(rsbinder_fmq::Error::BadValue("x")),
                StatusCode::BadValue
            );
            assert_eq!(
                StatusCode::from(rsbinder_fmq::Error::Corrupted("x")),
                StatusCode::BadValue
            );
            assert_eq!(
                StatusCode::from(rsbinder_fmq::Error::NoEventFlag),
                StatusCode::InvalidOperation
            );
            assert_eq!(
                StatusCode::from(rsbinder_fmq::Error::TimedOut),
                StatusCode::TimedOut
            );
            assert_eq!(
                StatusCode::from(rsbinder_fmq::Error::Os(rustix::io::Errno::NOMEM)),
                StatusCode::from(rustix::io::Errno::NOMEM)
            );
        }
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The platform primitives the queue is built on: a shared mapping, the
//! memfd that backs a queue this process creates, and the futex calls. Linux
//! and Android have them; every other target gets the same signatures
//! returning [`Error::Unsupported`], so the queue itself is written once.

use std::ptr::NonNull;
use std::sync::atomic::AtomicU32;

pub(crate) use rustix::time::Timespec;

use crate::error::{Error, Result};

/// One `mmap` of a grantor's region, from the page below its offset; unmapped on drop.
pub(crate) struct Mapping {
    base: NonNull<u8>,
    len: usize,
    delta: usize,
}

// SAFETY: an address range without thread affinity; users reach it via atomics/ordered copies.
unsafe impl Send for Mapping {}
unsafe impl Sync for Mapping {}

impl Mapping {
    /// The first byte of the grantor's region.
    pub(crate) fn ptr(&self) -> NonNull<u8> {
        // SAFETY: `delta < len` (`map` rejects an empty extent), so this stays in the mapping.
        unsafe { NonNull::new_unchecked(self.base.as_ptr().add(self.delta)) }
    }
}

/// Absolute `CLOCK_MONOTONIC` deadline for `FUTEX_WAIT_BITSET`; `None` for none or past the clock.
pub(crate) fn deadline_after(timeout: Option<std::time::Duration>) -> Result<Option<Timespec>> {
    let Some(timeout) = timeout else {
        return Ok(None);
    };
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    let Ok(secs) = i64::try_from(timeout.as_secs()) else {
        return Ok(None);
    };
    let add = Timespec {
        tv_sec: secs,
        tv_nsec: timeout.subsec_nanos() as _,
    };
    let deadline = now.checked_add(add);
    // Pre-5.1 kernels lack futex_time64: 32-bit targets pass `i32` seconds; past that, no deadline.
    #[cfg(target_pointer_width = "32")]
    let deadline = deadline.filter(|d| i32::try_from(d.tv_sec).is_ok());
    Ok(deadline)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use super::*;
    use rustix::fs::{FallocateFlags, MemfdFlags, SealFlags};
    use rustix::mm::{MapFlags, ProtFlags};
    use std::num::NonZeroU32;
    use std::os::fd::{BorrowedFd, OwnedFd};

    impl Mapping {
        pub(crate) fn map(fd: BorrowedFd<'_>, offset: u64, extent: u64) -> Result<Self> {
            let page = rustix::param::page_size() as u64;
            let map_offset = offset - offset % page;
            let delta = usize::try_from(offset - map_offset)
                .map_err(|_| Error::BadValue("grantor offset exceeds the address space"))?;
            let len = usize::try_from(extent)
                .ok()
                .and_then(|e| e.checked_add(delta))
                // A non-empty extent keeps `delta < len`, which `ptr` relies on.
                .filter(|_| extent > 0)
                .ok_or(Error::BadValue("grantor extent exceeds the address space"))?;
            // SAFETY: null hint, `len > 0`, `fd` open; this `Mapping` unmaps it once, in `Drop`.
            let raw = unsafe {
                rustix::mm::mmap(
                    std::ptr::null_mut(),
                    len,
                    ProtFlags::READ | ProtFlags::WRITE,
                    MapFlags::SHARED,
                    fd,
                    map_offset,
                )
            }?;
            let base = NonNull::new(raw.cast::<u8>()).ok_or(Error::Os(rustix::io::Errno::NOMEM))?;
            Ok(Self { base, len, delta })
        }
    }

    impl Drop for Mapping {
        fn drop(&mut self) {
            // SAFETY: `mmap`'s own `base`/`len`; derived pointers live only under owner borrows.
            let _ = unsafe { rustix::mm::munmap(self.base.as_ptr().cast(), self.len) };
        }
    }

    /// A sealed, fully allocated memfd of `total` bytes, page-rounded as libfmq rounds ashmem.
    pub(crate) fn create_shared(total: u64) -> Result<OwnedFd> {
        let page = rustix::param::page_size() as u64;
        let total = total
            .checked_add(page - 1)
            .map(|t| t & !(page - 1))
            .ok_or(Error::BadValue("queue too large"))?;
        let fd = rustix::fs::memfd_create(
            "rsbinder-fmq",
            MemfdFlags::CLOEXEC | MemfdFlags::ALLOW_SEALING,
        )?;
        rustix::fs::ftruncate(&fd, total)?;
        rustix::fs::fallocate(&fd, FallocateFlags::empty(), 0, total)?;
        rustix::fs::fcntl_add_seals(&fd, SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL)?;
        Ok(fd)
    }

    /// Non-private `FUTEX_WAIT_BITSET`; `Ok` on wake/`EAGAIN`/`EINTR`; caller rechecks the word.
    pub(crate) fn futex_wait(
        word: &AtomicU32,
        expected: u32,
        bits: u32,
        deadline: Option<&Timespec>,
    ) -> Result<()> {
        use rustix::io::Errno;
        use rustix::thread::futex::{wait_bitset, Flags};
        let bits = NonZeroU32::new(bits).ok_or(Error::BadValue("empty bit mask"))?;
        match wait_bitset(word, Flags::empty(), expected, deadline, bits) {
            Ok(()) => Ok(()),
            Err(Errno::AGAIN) | Err(Errno::INTR) => Ok(()),
            Err(Errno::TIMEDOUT) => Err(Error::TimedOut),
            Err(e) => Err(e.into()),
        }
    }

    /// `FUTEX_WAKE_BITSET` for every waiter whose mask intersects `bits`.
    pub(crate) fn futex_wake(word: &AtomicU32, bits: u32) -> Result<()> {
        use rustix::thread::futex::{wake_bitset, Flags};
        let bits = NonZeroU32::new(bits).ok_or(Error::BadValue("empty bit mask"))?;
        wake_bitset(word, Flags::empty(), i32::MAX as u32, bits)?;
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use super::*;
    use std::os::fd::{BorrowedFd, OwnedFd};

    impl Mapping {
        pub(crate) fn map(_fd: BorrowedFd<'_>, _offset: u64, _extent: u64) -> Result<Self> {
            // Keeps the fields alive for the type checker; never reached.
            let _ = (|m: &Mapping| (m.base, m.len, m.delta), Mapping::ptr);
            Err(Error::Unsupported)
        }
    }

    pub(crate) fn create_shared(_total: u64) -> Result<OwnedFd> {
        Err(Error::Unsupported)
    }

    pub(crate) fn futex_wait(
        _word: &AtomicU32,
        _expected: u32,
        _bits: u32,
        _deadline: Option<&Timespec>,
    ) -> Result<()> {
        Err(Error::Unsupported)
    }

    pub(crate) fn futex_wake(_word: &AtomicU32, _bits: u32) -> Result<()> {
        Err(Error::Unsupported)
    }
}

pub(crate) use imp::{create_shared, futex_wait, futex_wake};

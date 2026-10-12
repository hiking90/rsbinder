// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::AtomicU32;
use std::time::Duration;

use rustix::time::Timespec;

use crate::error::Result;

/// An absolute `CLOCK_MONOTONIC` instant for [`futex_wait`].
///
/// A wait that is retried — after a wake that was for someone else, after
/// `EINTR` — keeps one deadline rather than restarting a relative timeout on
/// every pass. `CLOCK_MONOTONIC` is the clock `FUTEX_WAIT_BITSET` measures
/// against by default; it does not advance while the system is suspended.
#[derive(Debug, Clone, Copy)]
pub struct Deadline(Timespec);

impl Deadline {
    /// The instant `timeout` from now.
    ///
    /// `None` when that instant cannot be expressed: past the end of the
    /// clock, or — on a 32-bit target, where the `futex` system call takes
    /// `i32` seconds and is the only one a kernel before Linux 5.1 or
    /// Android 10's seccomp filter offers — more than `i32::MAX` seconds of
    /// `CLOCK_MONOTONIC` time, about 68 years after boot. Such a deadline
    /// does not arrive within a process's life, so
    /// callers treat `None` as "no deadline":
    ///
    /// ```
    /// # use rsbinder_fmq::shm::Deadline;
    /// # let timeout = Some(std::time::Duration::from_millis(5));
    /// let deadline: Option<Deadline> = timeout.and_then(Deadline::after);
    /// ```
    ///
    /// Always `None` on WASI, where this crate has no `CLOCK_MONOTONIC` to
    /// read and [`futex_wait`] is unsupported.
    pub fn after(timeout: Duration) -> Option<Self> {
        let now = monotonic_now()?;
        let secs = i64::try_from(timeout.as_secs()).ok()?;
        let add = Timespec {
            tv_sec: secs,
            tv_nsec: timeout.subsec_nanos() as _,
        };
        let deadline = now.checked_add(add);
        // 32-bit `futex` takes `i32` seconds (pre-5.1 kernels, the Android path); past that, none.
        #[cfg(target_pointer_width = "32")]
        let deadline = deadline.filter(|d| i32::try_from(d.tv_sec).is_ok());
        deadline.map(Deadline)
    }
}

#[cfg(not(target_os = "wasi"))]
fn monotonic_now() -> Option<Timespec> {
    Some(rustix::time::clock_gettime(
        rustix::time::ClockId::Monotonic,
    ))
}

// rustix has no `clock_gettime` for WASI.
#[cfg(target_os = "wasi")]
fn monotonic_now() -> Option<Timespec> {
    None
}

#[cfg(any(target_os = "linux", target_os = "android"))]
mod imp {
    use super::*;
    use crate::error::Error;
    use std::num::NonZeroU32;

    pub fn futex_wait(
        word: &AtomicU32,
        expected: u32,
        bits: u32,
        deadline: Option<&Deadline>,
    ) -> Result<()> {
        use rustix::io::Errno;
        let bits = NonZeroU32::new(bits).ok_or(Error::BadValue("empty bit mask"))?;
        match wait_bitset(word, expected, deadline, bits) {
            Ok(()) => Ok(()),
            Err(Errno::AGAIN) | Err(Errno::INTR) => Ok(()),
            Err(Errno::TIMEDOUT) => Err(Error::TimedOut),
            Err(e) => Err(e.into()),
        }
    }

    #[inline]
    pub fn futex_wake(word: &AtomicU32, bits: u32) -> Result<()> {
        let bits = NonZeroU32::new(bits).ok_or(Error::BadValue("empty bit mask"))?;
        Ok(wake_bitset(word, bits)?)
    }

    #[cfg(not(all(target_os = "android", target_pointer_width = "32")))]
    fn wait_bitset(
        word: &AtomicU32,
        expected: u32,
        deadline: Option<&Deadline>,
        bits: NonZeroU32,
    ) -> rustix::io::Result<()> {
        use rustix::thread::futex::{wait_bitset, Flags};
        wait_bitset(word, Flags::empty(), expected, deadline.map(|d| &d.0), bits)
    }

    #[cfg(not(all(target_os = "android", target_pointer_width = "32")))]
    #[inline]
    fn wake_bitset(word: &AtomicU32, bits: NonZeroU32) -> rustix::io::Result<()> {
        use rustix::thread::futex::{wake_bitset, Flags};
        wake_bitset(word, Flags::empty(), i32::MAX as u32, bits).map(drop)
    }

    // API 29's seccomp filter answers `futex_time64`, rustix's first call here, with `SIGSYS`.
    #[cfg(all(target_os = "android", target_pointer_width = "32"))]
    fn wait_bitset(
        word: &AtomicU32,
        expected: u32,
        deadline: Option<&Deadline>,
        bits: NonZeroU32,
    ) -> rustix::io::Result<()> {
        // `Deadline::after` bounds `tv_sec` to `i32` on 32-bit targets, so the casts are exact.
        let timeout = deadline.map(|d| libc::timespec {
            tv_sec: d.0.tv_sec as libc::time_t,
            tv_nsec: d.0.tv_nsec as libc::c_long,
        });
        let timeout = timeout
            .as_ref()
            .map_or(std::ptr::null(), std::ptr::from_ref);
        // SAFETY: `word` is a live `AtomicU32`; `timeout` is null or the local above; no `uaddr2`.
        let r = unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAIT_BITSET,
                expected,
                timeout,
                std::ptr::null::<u32>(),
                bits.get(),
            )
        };
        if r < 0 {
            return Err(crate::error::errno_of(&std::io::Error::last_os_error()));
        }
        Ok(())
    }

    #[cfg(all(target_os = "android", target_pointer_width = "32"))]
    #[inline]
    fn wake_bitset(word: &AtomicU32, bits: NonZeroU32) -> rustix::io::Result<()> {
        // SAFETY: `word` is a live `AtomicU32`; the timeout and `uaddr2` are unused by WAKE_BITSET.
        let r = unsafe {
            libc::syscall(
                libc::SYS_futex,
                word.as_ptr(),
                libc::FUTEX_WAKE_BITSET,
                i32::MAX,
                std::ptr::null::<libc::timespec>(),
                std::ptr::null::<u32>(),
                bits.get(),
            )
        };
        if r < 0 {
            return Err(crate::error::errno_of(&std::io::Error::last_os_error()));
        }
        Ok(())
    }
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
mod imp {
    use super::*;
    use crate::error::Error;

    pub fn futex_wait(
        _word: &AtomicU32,
        _expected: u32,
        _bits: u32,
        deadline: Option<&Deadline>,
    ) -> Result<()> {
        // Reads the field, which nothing else on this target does.
        let _ = deadline.map(|d| d.0);
        Err(Error::Unsupported)
    }

    pub fn futex_wake(_word: &AtomicU32, _bits: u32) -> Result<()> {
        Err(Error::Unsupported)
    }
}

/// Sleep while `word` holds `expected`, until a [`futex_wake`] whose mask
/// intersects `bits`, or until `deadline` (`FUTEX_WAIT_BITSET`).
///
/// The futex is not private (`FUTEX_PRIVATE_FLAG` is not set): `word` is
/// normally in a shared mapping, and a waiter in one process is woken from
/// another. A read-only mapping is enough to wait on; the caller's own loads
/// of `word` are then `Relaxed` ones, the only atomic access
/// [`Access::ReadOnly`](super::Access::ReadOnly) allows.
///
/// `Ok` means "look again", not "the word changed": it is returned for a
/// wake, for a word that no longer held `expected` when the kernel checked
/// (`EAGAIN`), and for a signal (`EINTR`). The caller re-reads the word and
/// decides whether to wait again, with the same `deadline`.
///
/// # Errors
///
/// [`Error::TimedOut`](crate::Error::TimedOut) once `deadline` has passed;
/// [`Error::BadValue`](crate::Error::BadValue) for `bits == 0`; any other
/// failure of the system call as [`Error::Os`](crate::Error::Os);
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
pub fn futex_wait(
    word: &AtomicU32,
    expected: u32,
    bits: u32,
    deadline: Option<&Deadline>,
) -> Result<()> {
    imp::futex_wait(word, expected, bits, deadline)
}

/// Wake every [`futex_wait`] on `word` whose mask intersects `bits`
/// (`FUTEX_WAKE_BITSET`, not private).
///
/// This is one system call whether or not anyone is waiting; a protocol
/// that wants to skip it keeps its own "someone is waiting" state in the
/// word, as [`EventFlag`](crate::EventFlag) does. The kernel only reads the
/// address, so a waker whose mapping of the word is read-only works too.
///
/// # Errors
///
/// [`Error::BadValue`](crate::Error::BadValue) for `bits == 0`; any other
/// failure of the system call as [`Error::Os`](crate::Error::Os), which is
/// `ENOSYS` from a kernel before Linux 5.1 on the 32-bit targets the [module
/// documentation](super#platforms) describes;
/// [`Error::Unsupported`](crate::Error::Unsupported) off Linux and Android.
#[inline]
pub fn futex_wake(word: &AtomicU32, bits: u32) -> Result<()> {
    imp::futex_wake(word, bits)
}

#[cfg(all(test, any(target_os = "linux", target_os = "android")))]
mod tests {
    use super::*;
    use crate::Error;
    use std::time::Instant;

    #[test]
    fn a_wait_on_a_changed_word_returns_at_once() {
        let word = AtomicU32::new(1);
        futex_wait(&word, 0, 1, None).unwrap();
    }

    #[test]
    fn a_wait_times_out_at_its_deadline() {
        let word = AtomicU32::new(0);
        let started = Instant::now();
        let deadline = Deadline::after(Duration::from_millis(50));
        assert_eq!(
            futex_wait(&word, 0, 1, deadline.as_ref()).unwrap_err(),
            Error::TimedOut
        );
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn a_wake_reaches_only_a_waiter_whose_mask_intersects() {
        // The word never changes, so a waiter returns only for a wake or its deadline.
        let word = AtomicU32::new(0);
        std::thread::scope(|s| {
            let other = s.spawn(|| {
                let deadline = Deadline::after(Duration::from_millis(300));
                futex_wait(&word, 0, 0x2, deadline.as_ref())
            });
            let waiter = s.spawn(|| {
                let deadline = Deadline::after(Duration::from_secs(10));
                futex_wait(&word, 0, 0x1, deadline.as_ref())
            });
            // Repeated: the first wake may come before the waiter sleeps.
            while !waiter.is_finished() {
                futex_wake(&word, 0x1).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(waiter.join().unwrap(), Ok(()));
            // `other` may not have slept yet; a wake that ignored the mask would reach it here.
            for _ in 0..10 {
                futex_wake(&word, 0x1).unwrap();
                std::thread::sleep(Duration::from_millis(5));
            }
            assert_eq!(other.join().unwrap(), Err(Error::TimedOut));
        });
    }

    #[test]
    fn an_empty_mask_is_refused() {
        let word = AtomicU32::new(0);
        assert!(matches!(
            futex_wait(&word, 0, 0, None),
            Err(Error::BadValue(_))
        ));
        assert!(matches!(futex_wake(&word, 0), Err(Error::BadValue(_))));
    }

    #[test]
    fn a_timeout_past_the_clock_has_no_deadline() {
        assert!(Deadline::after(Duration::MAX).is_none());
        assert!(Deadline::after(Duration::from_secs(1)).is_some());
    }
}

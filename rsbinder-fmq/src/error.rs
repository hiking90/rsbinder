// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Why a queue, descriptor, flag or [`shm`](crate::shm) operation failed.
///
/// The `&'static str` payloads name the check that failed; they are for
/// logs and test assertions, not for matching on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// The target lacks what the call needs. The queue's constructors return
    /// this off Linux and Android, and on a target without 64-bit atomics;
    /// [`UnsyncReader::attach`](crate::UnsyncReader::attach) also where
    /// `shm::RoWord64::PRESENT` is false (32-bit x86 without SSE2); the
    /// memfd, seal and futex functions of [`shm`](crate::shm) off Linux and
    /// Android;
    /// [`Mapping::map`](crate::shm::Mapping::map) on a target
    /// without `mmap` (WASI).
    ///
    /// One case depends on the running kernel and not on the target:
    /// [`UnsyncWriter::create`](crate::UnsyncWriter::create) with
    /// [`read_only_readers`](field@crate::UnsyncCreate::read_only_readers)
    /// returns this on a kernel without `F_SEAL_FUTURE_WRITE`.
    Unsupported,
    /// An argument of the caller failed a check: a capacity, a count, an
    /// index into a transaction, an empty bit mask. The failing call moved
    /// no counter. A descriptor that came from a peer is
    /// [`InvalidDescriptor`](Self::InvalidDescriptor) instead.
    BadValue(&'static str),
    /// A descriptor given to [`MessageQueue::attach`](crate::MessageQueue::attach)
    /// or [`UnsyncReader::attach`](crate::UnsyncReader::attach) failed a
    /// check: its flavor, quantum or grantor geometry, one of its fds (not
    /// shrink-sealed, too small, a hugetlbfs file), or the
    /// [`AttachPolicy`](crate::AttachPolicy) (capacity above
    /// `max_capacity`, an fd without a seal, no EventFlag word). Nothing was
    /// mapped. The descriptor normally comes from the peer, so this is what
    /// a defective or hostile peer gets the receiver to return; the caller's
    /// own arguments are [`BadValue`](Self::BadValue).
    InvalidDescriptor(&'static str),
    /// The counters in shared memory violate the ring's invariant. The peer
    /// either has a defect or is hostile; the queue is unusable from here on.
    Corrupted(&'static str),
    /// A blocking operation on a queue whose descriptor carries no
    /// EventFlag word (three grantors instead of four).
    NoEventFlag,
    /// The wait's deadline passed.
    TimedOut,
    /// A system call failed.
    Os(Errno),
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// The error number (`errno`) a failed system call returned.
///
/// The value is the platform's own: compare it with the constants of
/// `libc` or `rustix` through [`raw`](Self::raw), and turn it into a
/// [`std::io::Error`] with `From` for its [`ErrorKind`](std::io::ErrorKind)
/// and message.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Errno(i32);

impl Errno {
    /// The error number `code`, as `errno` holds it.
    pub const fn from_raw(code: i32) -> Self {
        Errno(code)
    }

    /// The error number, as `errno` holds it.
    pub const fn raw(self) -> i32 {
        self.0
    }

    pub(crate) const fn of(e: rustix::io::Errno) -> Self {
        Errno(e.raw_os_error())
    }
}

pub(crate) const EINVAL: Errno = Errno::of(rustix::io::Errno::INVAL);

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&std::io::Error::from_raw_os_error(self.0), f)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&std::io::Error::from_raw_os_error(self.0), f)
    }
}

impl std::error::Error for Errno {}

impl From<Errno> for std::io::Error {
    fn from(e: Errno) -> Self {
        std::io::Error::from_raw_os_error(e.0)
    }
}

/// [`Error::Os`] of a rustix error; the crate's one way from rustix to [`Error`].
pub(crate) fn os(e: rustix::io::Errno) -> Error {
    Error::Os(Errno::of(e))
}

/// A peer descriptor's failure: `BadValue` from a check of what the descriptor names.
pub(crate) fn received(e: Error) -> Error {
    match e {
        Error::BadValue(what) => Error::InvalidDescriptor(what),
        other => other,
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => f.write_str(
                "not supported on this target or kernel: it lacks memfd, futex, mmap, \
                 64-bit atomics, a read-only 64-bit load or F_SEAL_FUTURE_WRITE",
            ),
            Error::BadValue(what) => write!(f, "rejected: {what}"),
            Error::InvalidDescriptor(what) => write!(f, "descriptor rejected: {what}"),
            Error::Corrupted(what) => write!(f, "shared counters corrupted: {what}"),
            Error::NoEventFlag => f.write_str("the queue has no EventFlag word"),
            Error::TimedOut => f.write_str("timed out"),
            Error::Os(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Os(e) => Some(e),
            _ => None,
        }
    }
}

/// `e`'s errno when it is in 1..4096, the range the kernel returns, else `EIO`.
pub(crate) fn errno_of(e: &std::io::Error) -> Errno {
    e.raw_os_error()
        .filter(|code| (1..4096).contains(code))
        .map_or(Errno::of(rustix::io::Errno::IO), Errno)
}

impl From<Error> for std::io::Error {
    fn from(e: Error) -> Self {
        use std::io::ErrorKind;
        match e {
            Error::Os(errno) => errno.into(),
            Error::TimedOut => std::io::Error::new(ErrorKind::TimedOut, e),
            Error::Unsupported => std::io::Error::new(ErrorKind::Unsupported, e),
            Error::BadValue(_) | Error::NoEventFlag => {
                std::io::Error::new(ErrorKind::InvalidInput, e)
            }
            Error::InvalidDescriptor(_) | Error::Corrupted(_) => {
                std::io::Error::new(ErrorKind::InvalidData, e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsupported_names_the_target_not_the_queue() {
        // `shm`'s functions return it too, to callers that have no queue.
        let text = Error::Unsupported.to_string();
        assert!(text.contains("this target"), "{text}");
        assert!(!text.to_lowercase().contains("queue"), "{text}");
    }

    #[test]
    fn an_errno_reads_as_the_io_error_of_its_number() {
        let perm = rustix::io::Errno::PERM.raw_os_error();
        let errno = Errno::from_raw(perm);
        assert_eq!(errno.raw(), perm);
        let io = std::io::Error::from(errno);
        assert_eq!(io.raw_os_error(), Some(perm));
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
        assert_eq!(errno.to_string(), io.to_string());
        assert_eq!(format!("{errno:?}"), format!("{io:?}"));
        let via_error = std::io::Error::from(Error::Os(errno));
        assert_eq!(via_error.raw_os_error(), Some(perm));
    }
}

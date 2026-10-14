// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Why a queue, descriptor, flag or [`shm`](crate::shm) operation failed.
///
/// The two `&'static str` payloads name the check that failed; they are for
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
    /// A descriptor, policy or argument failed a check; the failing call
    /// moved no counter, though a `write` may already have copied items into
    /// free ring space, which no reader sees without a commit. A commit given
    /// a correct `n` fails this way only when another party moved this side's
    /// counter, or the peer moved its own backwards, between begin and
    /// commit: a protocol breach this check cannot tell apart from a wrong
    /// `n`.
    BadValue(&'static str),
    /// The counters in shared memory violate the ring's invariant. The peer
    /// either has a defect or is hostile; the queue is unusable from here on.
    Corrupted(&'static str),
    /// A blocking operation on a queue whose descriptor carries no
    /// EventFlag word (three grantors instead of four).
    NoEventFlag,
    /// The wait's deadline passed.
    TimedOut,
    /// A system call failed.
    Os(rustix::io::Errno),
}

/// `Result` with this crate's [`Error`].
pub type Result<T> = std::result::Result<T, Error>;

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Unsupported => f.write_str(
                "not supported on this target or kernel: it lacks memfd, futex, mmap, \
                 64-bit atomics, a read-only 64-bit load or F_SEAL_FUTURE_WRITE",
            ),
            Error::BadValue(what) => write!(f, "rejected: {what}"),
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

impl From<rustix::io::Errno> for Error {
    fn from(e: rustix::io::Errno) -> Self {
        Error::Os(e)
    }
}

/// `e`'s errno when it is in 1..4096 (rustix linux_raw panics outside it), else `EIO`.
pub(crate) fn errno_of(e: &std::io::Error) -> rustix::io::Errno {
    e.raw_os_error()
        .filter(|code| (1..4096).contains(code))
        .map_or(rustix::io::Errno::IO, rustix::io::Errno::from_raw_os_error)
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
            Error::Corrupted(_) => std::io::Error::new(ErrorKind::InvalidData, e),
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
}

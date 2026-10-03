// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::fmt;

/// Why a queue, descriptor or flag operation failed.
///
/// The two `&'static str` payloads name the check that failed; they are for
/// logs and test assertions, not for matching on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Error {
    /// This target has no shared-memory or futex backend. Only Linux and
    /// Android are supported; elsewhere the crate compiles and every
    /// constructor returns this.
    Unsupported,
    /// A descriptor, policy or argument failed a check made before any
    /// shared memory was touched.
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
            Error::Unsupported => f.write_str("fast message queues need Linux or Android"),
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

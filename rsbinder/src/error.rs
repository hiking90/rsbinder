// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Error handling and status codes for binder operations.
//!
//! This module defines the result types and error codes used throughout
//! the binder library for consistent error handling across IPC operations.

use std::error::Error;
use std::fmt;

/// Result type alias for binder operations.
pub type Result<T> = std::result::Result<T, StatusCode>;

const UNKNOWN_ERROR: i32 = -2147483647 - 1;

/// Status codes for binder operations.
///
/// Represents various error conditions that can occur during binder IPC operations,
/// including system errors, protocol errors, and application-specific errors.
#[derive(Default, Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum StatusCode {
    /// Operation completed successfully
    #[default]
    Ok,
    /// Unknown error occurred
    Unknown,
    /// Out of memory
    NoMemory,
    /// Invalid operation for current state
    InvalidOperation,
    /// Invalid parameter value
    BadValue,
    /// Wrong data type
    BadType,
    /// Named resource not found
    NameNotFound,
    /// Permission denied
    PermissionDenied,
    /// Object not initialized
    NoInit,
    /// Resource already exists
    AlreadyExists,
    /// Remote object is dead
    DeadObject,
    /// Transaction failed
    FailedTransaction,
    /// Unknown transaction code
    UnknownTransaction,
    /// Invalid array index
    BadIndex,
    /// File descriptors not allowed
    FdsNotAllowed,
    /// Unexpected null pointer
    UnexpectedNull,
    /// Not enough data available
    NotEnoughData,
    /// Operation would block
    WouldBlock,
    /// Operation timed out
    TimedOut,
    /// Bad file descriptor
    BadFd,
    /// RPC transport/protocol error (binder-over-socket stack).
    ///
    /// Payload-free on purpose: `StatusCode` derives `Copy`/`Ord`/`Hash`
    /// and has three hand-written exhaustive matches, so a rich payload
    /// variant is impossible here. The detailed error lives in
    /// [`crate::rpc::RpcError`]; this variant is only the boundary
    /// projection used when an RPC failure must surface through
    /// `rsbinder::Result`. Present only with the `rpc` feature, so the
    /// default / rpc-off public API surface is unchanged.
    #[cfg(feature = "rpc")]
    RpcError,
    /// System errno value
    Errno(i32),
    /// Service-specific error code
    ServiceSpecific(i32),
}

impl Error for StatusCode {}

impl fmt::Display for StatusCode {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            StatusCode::Ok => write!(f, "Ok"),
            StatusCode::Unknown => write!(f, "Unknown"),
            StatusCode::NoMemory => write!(f, "NoMemory"),
            StatusCode::InvalidOperation => write!(f, "InvalidOperation"),
            StatusCode::BadValue => write!(f, "BadValue"),
            StatusCode::BadType => write!(f, "BadType"),
            StatusCode::NameNotFound => write!(f, "NameNotFound"),
            StatusCode::PermissionDenied => write!(f, "PermissionDenied"),
            StatusCode::NoInit => write!(f, "NoInit"),
            StatusCode::AlreadyExists => write!(f, "AlreadyExists"),
            StatusCode::DeadObject => write!(f, "DeadObject"),
            StatusCode::FailedTransaction => write!(f, "FailedTransaction"),
            StatusCode::UnknownTransaction => write!(f, "UnknownTransaction"),
            StatusCode::BadIndex => write!(f, "BadIndex"),
            StatusCode::FdsNotAllowed => write!(f, "FdsNotAllowed"),
            StatusCode::UnexpectedNull => write!(f, "UnexpectedNull"),
            StatusCode::NotEnoughData => write!(f, "NotEnoughData"),
            StatusCode::WouldBlock => write!(f, "WouldBlock"),
            StatusCode::TimedOut => write!(f, "TimedOut"),
            StatusCode::BadFd => write!(f, "BadFd"),
            #[cfg(feature = "rpc")]
            StatusCode::RpcError => write!(f, "RpcError"),
            StatusCode::Errno(errno) => write!(f, "Errno({errno})"),
            StatusCode::ServiceSpecific(v) => write!(f, "ServiceSpecific({v})"),
        }
    }
}

impl From<StatusCode> for i32 {
    fn from(code: StatusCode) -> Self {
        match code {
            StatusCode::Ok => 0,
            StatusCode::Unknown => UNKNOWN_ERROR as _,
            StatusCode::NoMemory => -(rustix::io::Errno::NOMEM.raw_os_error()),
            StatusCode::InvalidOperation => -(rustix::io::Errno::NOSYS.raw_os_error()),
            StatusCode::BadValue => -(rustix::io::Errno::INVAL.raw_os_error()),
            StatusCode::BadType => UNKNOWN_ERROR + 1,
            StatusCode::NameNotFound => -(rustix::io::Errno::NOENT.raw_os_error()),
            StatusCode::PermissionDenied => -(rustix::io::Errno::PERM.raw_os_error()),
            StatusCode::NoInit => -(rustix::io::Errno::NODEV.raw_os_error()),
            StatusCode::AlreadyExists => -(rustix::io::Errno::EXIST.raw_os_error()),
            StatusCode::DeadObject => -(rustix::io::Errno::PIPE.raw_os_error()),
            StatusCode::FailedTransaction => UNKNOWN_ERROR + 2,
            StatusCode::UnknownTransaction => -(rustix::io::Errno::BADMSG.raw_os_error()),
            StatusCode::BadIndex => -(rustix::io::Errno::OVERFLOW.raw_os_error()),
            StatusCode::FdsNotAllowed => UNKNOWN_ERROR + 7,
            StatusCode::UnexpectedNull => UNKNOWN_ERROR + 8,
            // `UNKNOWN_ERROR + 9` is AOSP `FROZEN_OBJECT` (Errors.h); keep
            // `RpcError` off that slot so an incoming kernel `FROZEN_OBJECT`
            // status is not mis-decoded as an RPC transport error (and so a
            // stray `RpcError` on a `status_t` wire is not read as "frozen" by a
            // C++ peer). `+ 10` is unused by AOSP's status_t table.
            #[cfg(feature = "rpc")]
            StatusCode::RpcError => UNKNOWN_ERROR + 10,
            StatusCode::NotEnoughData => -(rustix::io::Errno::NODATA.raw_os_error()),
            StatusCode::WouldBlock => -(rustix::io::Errno::WOULDBLOCK.raw_os_error()),
            StatusCode::TimedOut => -(rustix::io::Errno::TIMEDOUT.raw_os_error()),
            StatusCode::BadFd => -(rustix::io::Errno::BADF.raw_os_error()),
            StatusCode::ServiceSpecific(v) => v,
            StatusCode::Errno(errno) => errno,
        }
    }
}

impl From<i32> for StatusCode {
    fn from(code: i32) -> Self {
        // Lifting the forward map's values into `const`s (each is
        // `const`-evaluable: `Errno::raw_os_error` is `const fn` and
        // `UNKNOWN_ERROR + N` are integer constants) lets this match
        // lower to a jump-table / equality cascade instead of a linear
        // chain of `code == <variant>.into()` guards. Per-platform errno
        // mapping is preserved.
        use rustix::io::Errno;
        const OK: i32 = 0;
        const UNKNOWN: i32 = UNKNOWN_ERROR;
        const NO_MEMORY: i32 = -Errno::NOMEM.raw_os_error();
        const INVALID_OPERATION: i32 = -Errno::NOSYS.raw_os_error();
        const BAD_VALUE: i32 = -Errno::INVAL.raw_os_error();
        const BAD_TYPE: i32 = UNKNOWN_ERROR + 1;
        const NAME_NOT_FOUND: i32 = -Errno::NOENT.raw_os_error();
        const PERMISSION_DENIED: i32 = -Errno::PERM.raw_os_error();
        const NO_INIT: i32 = -Errno::NODEV.raw_os_error();
        const ALREADY_EXISTS: i32 = -Errno::EXIST.raw_os_error();
        const DEAD_OBJECT: i32 = -Errno::PIPE.raw_os_error();
        const FAILED_TRANSACTION: i32 = UNKNOWN_ERROR + 2;
        const UNKNOWN_TRANSACTION: i32 = -Errno::BADMSG.raw_os_error();
        const BAD_INDEX: i32 = -Errno::OVERFLOW.raw_os_error();
        const FDS_NOT_ALLOWED: i32 = UNKNOWN_ERROR + 7;
        const UNEXPECTED_NULL: i32 = UNKNOWN_ERROR + 8;
        // Off AOSP `FROZEN_OBJECT` (`UNKNOWN_ERROR + 9`); see the forward map.
        #[cfg(feature = "rpc")]
        const RPC_ERROR: i32 = UNKNOWN_ERROR + 10;
        const NOT_ENOUGH_DATA: i32 = -Errno::NODATA.raw_os_error();
        const WOULD_BLOCK: i32 = -Errno::WOULDBLOCK.raw_os_error();
        const TIMED_OUT: i32 = -Errno::TIMEDOUT.raw_os_error();
        const BAD_FD: i32 = -Errno::BADF.raw_os_error();

        match code {
            OK => StatusCode::Ok,
            UNKNOWN => StatusCode::Unknown,
            NO_MEMORY => StatusCode::NoMemory,
            INVALID_OPERATION => StatusCode::InvalidOperation,
            BAD_VALUE => StatusCode::BadValue,
            BAD_TYPE => StatusCode::BadType,
            NAME_NOT_FOUND => StatusCode::NameNotFound,
            PERMISSION_DENIED => StatusCode::PermissionDenied,
            NO_INIT => StatusCode::NoInit,
            ALREADY_EXISTS => StatusCode::AlreadyExists,
            DEAD_OBJECT => StatusCode::DeadObject,
            FAILED_TRANSACTION => StatusCode::FailedTransaction,
            UNKNOWN_TRANSACTION => StatusCode::UnknownTransaction,
            BAD_INDEX => StatusCode::BadIndex,
            FDS_NOT_ALLOWED => StatusCode::FdsNotAllowed,
            UNEXPECTED_NULL => StatusCode::UnexpectedNull,
            #[cfg(feature = "rpc")]
            RPC_ERROR => StatusCode::RpcError,
            NOT_ENOUGH_DATA => StatusCode::NotEnoughData,
            WOULD_BLOCK => StatusCode::WouldBlock,
            TIMED_OUT => StatusCode::TimedOut,
            BAD_FD => StatusCode::BadFd,
            x if x < 0 => StatusCode::Errno(x),
            x => StatusCode::ServiceSpecific(x),
        }
    }
}

impl From<std::array::TryFromSliceError> for StatusCode {
    fn from(_: std::array::TryFromSliceError) -> Self {
        StatusCode::NotEnoughData
    }
}

impl From<std::io::Error> for StatusCode {
    fn from(err: std::io::Error) -> Self {
        // Preserve the underlying errno instead of flattening every I/O
        // failure to BadFd. OS-backed errors route through the errno
        // mapping (BADF still yields BadFd); non-OS errors have no errno
        // to map, so they surface as Unknown.
        match err.raw_os_error() {
            Some(raw) => StatusCode::from(rustix::io::Errno::from_raw_os_error(raw)),
            None => StatusCode::Unknown,
        }
    }
}

impl From<rustix::io::Errno> for StatusCode {
    fn from(errno: rustix::io::Errno) -> Self {
        match errno {
            rustix::io::Errno::NOMEM => StatusCode::NoMemory,
            rustix::io::Errno::NOSYS => StatusCode::InvalidOperation,
            rustix::io::Errno::INVAL => StatusCode::BadValue,
            rustix::io::Errno::NOENT => StatusCode::NameNotFound,
            rustix::io::Errno::PERM => StatusCode::PermissionDenied,
            rustix::io::Errno::NODEV => StatusCode::NoInit,
            rustix::io::Errno::EXIST => StatusCode::AlreadyExists,
            rustix::io::Errno::PIPE => StatusCode::DeadObject,
            rustix::io::Errno::BADMSG => StatusCode::UnknownTransaction,
            rustix::io::Errno::OVERFLOW => StatusCode::BadIndex,
            rustix::io::Errno::NODATA => StatusCode::NotEnoughData,
            rustix::io::Errno::WOULDBLOCK => StatusCode::WouldBlock,
            rustix::io::Errno::TIMEDOUT => StatusCode::TimedOut,
            rustix::io::Errno::BADF => StatusCode::BadFd,
            _ => StatusCode::Errno(-errno.raw_os_error()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_code() {
        let code = StatusCode::Ok;
        assert_eq!(code, StatusCode::from(0));
        assert_eq!(code, StatusCode::from(Into::<i32>::into(StatusCode::Ok)));

        let code = StatusCode::Unknown;
        assert_eq!(code, StatusCode::from(UNKNOWN_ERROR));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::Unknown))
        );

        let code = StatusCode::NoMemory;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::NOMEM.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::NoMemory))
        );

        let code = StatusCode::InvalidOperation;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::NOSYS.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::InvalidOperation))
        );

        let code = StatusCode::BadValue;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::INVAL.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::BadValue))
        );

        let code = StatusCode::BadType;
        assert_eq!(code, StatusCode::from(UNKNOWN_ERROR + 1));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::BadType))
        );

        let code = StatusCode::NameNotFound;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::NOENT.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::NameNotFound))
        );

        let code = StatusCode::PermissionDenied;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::PERM.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::PermissionDenied))
        );

        let code = StatusCode::NoInit;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::NODEV.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::NoInit))
        );

        let code = StatusCode::AlreadyExists;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::EXIST.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::AlreadyExists))
        );

        let code = StatusCode::DeadObject;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::PIPE.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::DeadObject))
        );

        let code = StatusCode::FailedTransaction;
        assert_eq!(code, StatusCode::from(UNKNOWN_ERROR + 2));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::FailedTransaction))
        );

        let code = StatusCode::UnknownTransaction;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::BADMSG.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::UnknownTransaction))
        );

        let code = StatusCode::BadIndex;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::OVERFLOW.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::BadIndex))
        );

        let code = StatusCode::FdsNotAllowed;
        assert_eq!(code, StatusCode::from(UNKNOWN_ERROR + 7));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::FdsNotAllowed))
        );

        let code = StatusCode::UnexpectedNull;
        assert_eq!(code, StatusCode::from(UNKNOWN_ERROR + 8));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::UnexpectedNull))
        );

        let code = StatusCode::NotEnoughData;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::NODATA.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::NotEnoughData))
        );

        let code = StatusCode::WouldBlock;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::WOULDBLOCK.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::WouldBlock))
        );

        let code = StatusCode::TimedOut;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::TIMEDOUT.raw_os_error()))
        );
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::TimedOut))
        );

        let code = StatusCode::BadFd;
        assert_eq!(
            code,
            StatusCode::from(-(rustix::io::Errno::BADF.raw_os_error()))
        );
        assert_eq!(code, StatusCode::from(Into::<i32>::into(StatusCode::BadFd)));

        let code = StatusCode::ServiceSpecific(1);
        assert_eq!(code, StatusCode::from(1));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::ServiceSpecific(1)))
        );

        let code: StatusCode = StatusCode::Errno(-64);
        assert_eq!(code, StatusCode::from(-64));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::Errno(-64)))
        );
    }

    #[test]
    fn test_status_code_from_errno() {
        let code = StatusCode::from(rustix::io::Errno::NOMEM);
        assert_eq!(code, StatusCode::NoMemory);

        let code = StatusCode::from(rustix::io::Errno::NOSYS);
        assert_eq!(code, StatusCode::InvalidOperation);

        let code = StatusCode::from(rustix::io::Errno::INVAL);
        assert_eq!(code, StatusCode::BadValue);

        let code = StatusCode::from(rustix::io::Errno::NOENT);
        assert_eq!(code, StatusCode::NameNotFound);

        let code = StatusCode::from(rustix::io::Errno::PERM);
        assert_eq!(code, StatusCode::PermissionDenied);

        let code = StatusCode::from(rustix::io::Errno::NODEV);
        assert_eq!(code, StatusCode::NoInit);

        let code = StatusCode::from(rustix::io::Errno::EXIST);
        assert_eq!(code, StatusCode::AlreadyExists);

        let code = StatusCode::from(rustix::io::Errno::PIPE);
        assert_eq!(code, StatusCode::DeadObject);

        let code = StatusCode::from(rustix::io::Errno::BADMSG);
        assert_eq!(code, StatusCode::UnknownTransaction);

        let code = StatusCode::from(rustix::io::Errno::OVERFLOW);
        assert_eq!(code, StatusCode::BadIndex);

        let code = StatusCode::from(rustix::io::Errno::NODATA);
        assert_eq!(code, StatusCode::NotEnoughData);

        let code = StatusCode::from(rustix::io::Errno::WOULDBLOCK);
        assert_eq!(code, StatusCode::WouldBlock);

        let code = StatusCode::from(rustix::io::Errno::TIMEDOUT);
        assert_eq!(code, StatusCode::TimedOut);

        let code = StatusCode::from(rustix::io::Errno::BADF);
        assert_eq!(code, StatusCode::BadFd);

        let code = StatusCode::from(rustix::io::Errno::from_raw_os_error(64));
        assert_eq!(code, StatusCode::Errno(-64));
    }

    // Pin the round-trip between the forward `From<StatusCode> for i32`
    // and the reverse `From<i32> for StatusCode`. Each named variant
    // must come back through the const-pattern match identically,
    // otherwise the const initialisers diverged from the forward map's
    // calls into `rustix::io::Errno::*.raw_os_error()`. Run on every
    // target (macOS / Linux / Android) since the errno wire values
    // differ per OS.
    #[test]
    fn from_i32_round_trip_pins_every_named_variant() {
        let variants = [
            StatusCode::Ok,
            StatusCode::Unknown,
            StatusCode::NoMemory,
            StatusCode::InvalidOperation,
            StatusCode::BadValue,
            StatusCode::BadType,
            StatusCode::NameNotFound,
            StatusCode::PermissionDenied,
            StatusCode::NoInit,
            StatusCode::AlreadyExists,
            StatusCode::DeadObject,
            StatusCode::FailedTransaction,
            StatusCode::UnknownTransaction,
            StatusCode::BadIndex,
            StatusCode::FdsNotAllowed,
            StatusCode::UnexpectedNull,
            #[cfg(feature = "rpc")]
            StatusCode::RpcError,
            StatusCode::NotEnoughData,
            StatusCode::WouldBlock,
            StatusCode::TimedOut,
            StatusCode::BadFd,
        ];
        for v in variants {
            let wire: i32 = v.into();
            let back: StatusCode = wire.into();
            assert_eq!(
                back, v,
                "round-trip mismatch for {v:?} (wire={wire}) — \
                 the const initialiser for this arm has drifted from \
                 the forward `From<StatusCode> for i32` impl"
            );
        }
    }

    // Pre-existing convention preserved post-refactor: a *positive*
    // i32 that doesn't match any named status falls into
    // `ServiceSpecific`; a *negative* one falls into `Errno`. The
    // refactor's wildcard arms (`x if x < 0 => Errno(x)` and
    // `x => ServiceSpecific(x)`) lock that boundary.
    #[test]
    fn from_i32_unrecognized_routes_to_service_specific_or_errno() {
        assert_eq!(StatusCode::from(12345), StatusCode::ServiceSpecific(12345));
        assert_eq!(StatusCode::from(1), StatusCode::ServiceSpecific(1));
        // Pick a negative value the refactor's const map doesn't claim.
        // `-999` is not in any AOSP errno table.
        assert_eq!(StatusCode::from(-999), StatusCode::Errno(-999));
    }

    // Regression: `From<std::io::Error>` must preserve the underlying
    // errno through the Errno mapping instead of flattening every I/O
    // failure to `BadFd`. Non-OS errors (no errno) surface as `Unknown`.
    #[test]
    fn status_code_from_io_error_preserves_errno() {
        let enoent = std::io::Error::from_raw_os_error(rustix::io::Errno::NOENT.raw_os_error());
        assert_eq!(
            StatusCode::from(enoent),
            StatusCode::NameNotFound,
            "ENOENT must map to NameNotFound, not be flattened to BadFd"
        );

        // BADF still resolves to BadFd — via the errno mapping, not a blanket default.
        let ebadf = std::io::Error::from_raw_os_error(rustix::io::Errno::BADF.raw_os_error());
        assert_eq!(StatusCode::from(ebadf), StatusCode::BadFd);

        // A non-OS error has no errno to map → Unknown.
        let non_os = std::io::Error::other("no errno");
        assert_eq!(non_os.raw_os_error(), None);
        assert_eq!(StatusCode::from(non_os), StatusCode::Unknown);
    }
}

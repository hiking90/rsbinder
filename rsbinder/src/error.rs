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
        match code {
            code if code == StatusCode::Ok.into() => StatusCode::Ok,
            code if code == StatusCode::Unknown.into() => StatusCode::Unknown,
            code if code == StatusCode::NoMemory.into() => StatusCode::NoMemory,
            code if code == StatusCode::InvalidOperation.into() => StatusCode::InvalidOperation,
            code if code == StatusCode::BadValue.into() => StatusCode::BadValue,
            code if code == StatusCode::BadType.into() => StatusCode::BadType,
            code if code == StatusCode::NameNotFound.into() => StatusCode::NameNotFound,
            code if code == StatusCode::PermissionDenied.into() => StatusCode::PermissionDenied,
            code if code == StatusCode::NoInit.into() => StatusCode::NoInit,
            code if code == StatusCode::AlreadyExists.into() => StatusCode::AlreadyExists,
            code if code == StatusCode::DeadObject.into() => StatusCode::DeadObject,
            code if code == StatusCode::FailedTransaction.into() => StatusCode::FailedTransaction,
            code if code == StatusCode::UnknownTransaction.into() => StatusCode::UnknownTransaction,
            code if code == StatusCode::BadIndex.into() => StatusCode::BadIndex,
            code if code == StatusCode::FdsNotAllowed.into() => StatusCode::FdsNotAllowed,
            code if code == StatusCode::UnexpectedNull.into() => StatusCode::UnexpectedNull,
            code if code == StatusCode::NotEnoughData.into() => StatusCode::NotEnoughData,
            code if code == StatusCode::WouldBlock.into() => StatusCode::WouldBlock,
            code if code == StatusCode::TimedOut.into() => StatusCode::TimedOut,
            code if code == StatusCode::BadFd.into() => StatusCode::BadFd,
            code if code < 0 => StatusCode::Errno(code),
            _ => StatusCode::ServiceSpecific(code),
        }
    }
}

impl From<std::array::TryFromSliceError> for StatusCode {
    fn from(_: std::array::TryFromSliceError) -> Self {
        StatusCode::NotEnoughData
    }
}

impl From<std::io::Error> for StatusCode {
    fn from(_: std::io::Error) -> Self {
        StatusCode::BadFd
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
}

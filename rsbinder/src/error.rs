// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::fmt;
use std::error::Error;

pub type Result<T> = std::result::Result<T, StatusCode>;

const UNKNOWN_ERROR: i32 = -2147483647-1;

#[derive(Default, Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum StatusCode {
    #[default]
    Ok,
    Unknown,
    NoMemory,
    InvalidOperation,
    BadValue,
    BadType,
    NameNotFound,
    PermissionDenied,
    NoInit,
    AlreadyExists,
    DeadObject,
    FailedTransaction,
    UnknownTransaction,
    BadIndex,
    FdsNotAllowed,
    UnexpectedNull,
    NotEnoughData,
    WouldBlock,
    TimedOut,
    BadFd,
    Errno(i32),
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
            StatusCode::NoMemory => -libc::ENOMEM as _,
            StatusCode::InvalidOperation => -libc::ENOSYS as _,
            StatusCode::BadValue => -libc::EINVAL as _,
            StatusCode::BadType => UNKNOWN_ERROR + 1,
            StatusCode::NameNotFound => -libc::ENOENT as _,
            StatusCode::PermissionDenied => -libc::EPERM as _,
            StatusCode::NoInit => -libc::ENODEV as _,
            StatusCode::AlreadyExists => -libc::EEXIST as _,
            StatusCode::DeadObject => -libc::EPIPE as _,
            StatusCode::FailedTransaction => UNKNOWN_ERROR + 2,
            StatusCode::UnknownTransaction => -libc::EBADMSG as _,
            StatusCode::BadIndex => -libc::EOVERFLOW as _,
            StatusCode::FdsNotAllowed => UNKNOWN_ERROR + 7,
            StatusCode::UnexpectedNull => UNKNOWN_ERROR + 8,
            StatusCode::NotEnoughData => -libc::ENODATA as _,
            StatusCode::WouldBlock => -libc::EWOULDBLOCK as _,
            StatusCode::TimedOut => -libc::ETIMEDOUT as _,
            StatusCode::BadFd => -libc::EBADF as _,
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

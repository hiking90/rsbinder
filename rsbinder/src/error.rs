// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::string::FromUtf16Error;
use std::error;
// use std::string::FromUtf16Error;
// use std::array::TryFromSliceError;


// use thiserror;

use crate::parcelable::*;
use crate::parcel::*;

pub type Result<T> = std::result::Result<T, Error>;
// pub type Status<T> = std::result::Result<T, Exception>;

#[derive(Debug)]
pub enum Error {
    Status(Status),
    Any(Box<dyn error::Error>),
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::Status(status1), Error::Status(status2)) => status1 == status2,
            _ => false
        }
    }
}

impl From<FromUtf16Error> for Error {
    fn from(err: FromUtf16Error) -> Self {
        Error::Any(err.into())
    }
}


const UNKNOWN_ERROR: isize = -2147483647-1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum StatusCode {
    Ok = 0,
    Unknown = UNKNOWN_ERROR,
    NoMemory = -libc::ENOMEM as _,
    InvalidOperation = -libc::ENOSYS as _,
    BadValue = -libc::EINVAL as _,
    BadType = UNKNOWN_ERROR + 1,
    NameNotFound = -libc::ENOENT as _,
    PermissionDenied = -libc::EPERM as _,
    NoInit = -libc::ENODEV as _,
    AlreadyExists = -libc::EEXIST as _,
    DeadObject = -libc::EPIPE as _,
    FailedTransaction = UNKNOWN_ERROR + 2,
    UnknownTransaction = -libc::EBADMSG as _,
    BadIndex = -libc::EOVERFLOW as _,
    FdsNotAllowed = UNKNOWN_ERROR + 7,
    UnexpectedNull = UNKNOWN_ERROR + 8,
    NotEnoughData = -libc::ENODATA as _,
    WouldBlock = -libc::EWOULDBLOCK as _,
    TimedOut = -libc::ETIMEDOUT as _,
    BadFd = -libc::EBADF as _,
    ServiceSpecific = -8,
}

impl From<StatusCode> for Error {
    fn from(kind: StatusCode) -> Self {
        Error::Status(Status {
            status_code: kind as _,
            exception_code: ExceptionCode::None as _,
            message: format!("StatusCode: {:?}", kind),
        })
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ExceptionCode {
    None = 0,
    Security = -1,
    BadParcelable = -2,
    IllegalArgument = -3,
    NullPointer = -4,
    IllegalState = -5,
    NetworkMainThread = -6,
    UnsupportedOperation = -7,
    ServiceSpecific = -8,
    Parcelable = -9,

// This is special and Java specific; see Parcel.java.
    HasReplyHeader = -128,
// This is special, and indicates to C++ binder proxies that the
// transaction has failed at a low level.
    TransactionFailed = -129,
    JustError = -256,
}

impl From<ExceptionCode> for Error {
    fn from(kind: ExceptionCode) -> Self {
        Error::Status(Status {
            status_code: StatusCode::Ok as _,
            exception_code: kind as _,
            message: format!("ExceptionCode: {:?}", kind),
        })
    }
}

#[derive(PartialEq, Debug, Clone)]
pub struct Status {
    pub status_code: i32,
    pub exception_code: i32,
    pub message: String,
}

impl Status {
    pub fn new(status_code: StatusCode, exception_code: ExceptionCode, message: &str) -> Self {
        Status {
            status_code: status_code as _,
            exception_code: exception_code as _,
            message: message.into(),
        }
    }

    pub fn from_i32_status(status_code: i32, exception_code: ExceptionCode, message: &str) -> Self {
        Status {
            status_code,
            exception_code: exception_code as _,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Exception (code: {}, exception: {}): {}", self.status_code, self.exception_code, self.message)
    }
}

impl Serialize for Status {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write::<i32>(&self.exception_code)
    }
}

impl Deserialize for Status {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let exception = parcel.read::<i32>()?;

        if exception == ExceptionCode::None as _ {
            Ok(Status {
                status_code: StatusCode::Ok as _,
                exception_code: exception,
                message: "Deserialize Status".to_owned(),
            })
        } else {
            Err(Error::Status(Status {
                status_code: StatusCode::Ok as _,
                exception_code: exception,
                message: "Deserialize Status".to_owned(),
            }))
        }
    }
}


impl From<Status> for Error {
    fn from(status: Status) -> Self {
        Error::Status(status)
    }
}


// impl error::Error for Status {}

impl<T> Serialize for Result<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        match self {
            Ok(_) => {
                parcel.write::<i32>(&0)?;
            }
            Err(err) => {
                let code = match err {
                    Error::Status(status) => {
                        if status.exception_code == ExceptionCode::TransactionFailed as i32 {
                            return Err(Error::Status(status.clone()))
                        }

                        parcel.write::<i32>(&status.exception_code)?;
                        if status.exception_code == ExceptionCode::None as i32 {
                            return Ok(())
                        }

                        parcel.write(&String16(status.message.clone()))?;

                        if status.exception_code == ExceptionCode::ServiceSpecific as i32 {
                            status.status_code
                        } else {
                            0
                        }
                    },
                    _ => {
                        parcel.write::<i32>(&(ExceptionCode::JustError as i32))?;
                        let message = format!("{:?}", err);
                        parcel.write(&String16(message))?;

                        0
                    },
                };

                parcel.write::<i32>(&0)?;
                parcel.write::<i32>(&code)?;
            }
        }

        Ok(())
    }
}

impl Deserialize for Result<()> {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let exception = parcel.read::<i32>()?;

        let status = if exception == ExceptionCode::None as i32 {
            Ok(())
        } else {
            let message = parcel.read::<String16>()?;
            _ = parcel.read::<i32>()?;
            let code = parcel.read::<i32>()?;

            Err(Status {
                status_code: code,
                exception_code: exception,
                message: message.0,
            }.into())
        };

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_parcelable() -> Result<()> {
        let ok = Ok(());
        let illegal_status = Result::<()>::Err(
            Status::new(StatusCode::Ok, ExceptionCode::IllegalArgument, "IllegalArgument").into());
        let failed_status = Result::<()>::Err(
            Status::new(StatusCode::Ok, ExceptionCode::TransactionFailed, "TransactionFailed").into());
        let service_specific_status = Result::<()>::Err(
            Status::new(StatusCode::NameNotFound,
                ExceptionCode::ServiceSpecific, "IllegalArgument").into());

        let mut parcel = Parcel::new();

        {
            parcel.write(&ok)?;
            parcel.write(&illegal_status)?;
            parcel.write(&service_specific_status)?;
            assert!(parcel.write(&failed_status).is_err());
        }

        {
            assert_eq!(parcel.read::<Result<()>>()?, ok);
            assert_eq!(parcel.read::<Result<()>>()?, illegal_status);
            assert_eq!(parcel.read::<Result<()>>()?, service_specific_status);
            assert!(parcel.read::<Result<()>>().is_err());
        }

        Ok(())
    }
}
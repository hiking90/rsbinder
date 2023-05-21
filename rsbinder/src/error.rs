use std::error;
use std::string::FromUtf16Error;
use std::array::TryFromSliceError;

use libc;
use thiserror;

use crate::parcelable::*;
use crate::parcel::*;

pub type Result<T> = std::result::Result<T, Error>;
// pub type Status<T> = std::result::Result<T, Exception>;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("IO error: {0:?}")]
    Io(#[from] std::io::Error),

    #[error("Exception error: {0:?}")]
    Exception(#[from] Exception),

    #[error("Errno: {0:?}")]
    Errno(#[from] nix::errno::Errno),

    #[error("String error: {0:?}")]
    Encoding(#[from] FromUtf16Error),

    #[error("Array error: {0:?}")]
    Slice(#[from] TryFromSliceError),
}

impl PartialEq for Error {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Error::Io(err1), Error::Io(err2)) => err1.kind() == err2.kind(),
            (Error::Exception(err1), Error::Exception(err2)) => err1 == err2,
            (Error::Errno(err1), Error::Errno(err2)) => err1 == err2,
            (Error::Encoding(_), Error::Encoding(_)) => true,
            (Error::Slice(_), Error::Slice(_)) => true,
            _ => false
        }
    }
}

const UNKNOWN_ERROR: isize = -2147483647-1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ErrorKind {
    NoError = 0,
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

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Self {
        Exception {
            code: kind as _,
            exception: ErrorKind::ServiceSpecific as _,
            message: format!("ErrorKind: {:?}", kind),
        }.into()
    }
}

#[derive(Clone, Copy, Debug)]
pub enum ExceptionKind {
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

#[derive(PartialEq, Debug, Clone)]
pub struct Exception {
    pub code: i32,
    pub exception: i32,
    pub message: String,
}

impl Exception {
    pub fn new(code: i32, exception: ExceptionKind, message: String) -> Self {
        Exception {
            code: code,
            exception: exception as _,
            message: message,
        }
    }
}

impl From<ExceptionKind> for Error {
    fn from(kind: ExceptionKind) -> Self {
        Exception {
            code: 0,
            exception: kind as _,
            message: format!("ExceptionKind: {:?}", kind),
        }.into()
    }
}

impl std::fmt::Display for Exception {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Exception (code: {}, exception: {}): {}", self.code, self.exception, self.message)
    }
}

impl error::Error for Exception {}

impl<T> Serialize for Result<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        match self {
            Ok(_) => {
                parcel.write::<i32>(&0)?;
            }
            Err(err) => {
                let code = match err {
                    Error::Exception(exception) => {
                        if exception.exception == ExceptionKind::TransactionFailed as i32 {
                            return Err(Error::Exception(exception.clone()))
                        }

                        parcel.write::<i32>(&exception.exception)?;
                        if exception.exception == ExceptionKind::None as i32 {
                            return Ok(())
                        }

                        parcel.write(&String16(exception.message.clone()))?;

                        if exception.exception == ExceptionKind::ServiceSpecific as i32 {
                            exception.code
                        } else {
                            0
                        }
                    },
                    _ => {
                        parcel.write::<i32>(&(ExceptionKind::JustError as i32))?;
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

        let status = if exception == ExceptionKind::None as i32 {
            Ok(())
        } else {
            let message = parcel.read::<String16>()?;
            _ = parcel.read::<i32>()?;
            let code = parcel.read::<i32>()?;

            Err(Exception {
                code: code,
                exception: exception,
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
            Exception::new(0, ExceptionKind::IllegalArgument as _, "IllegalArgument".to_owned()).into());
        let failed_status = Result::<()>::Err(
            Exception::new(0, ExceptionKind::TransactionFailed as _, "TransactionFailed".to_owned()).into());
        let service_specific_status = Result::<()>::Err(
            Exception::new(ErrorKind::NameNotFound as _,
                ExceptionKind::ServiceSpecific as _, "IllegalArgument".to_owned()).into());

        let mut parcel = Parcel::new();

        {
            parcel.write(&ok)?;
            parcel.write(&illegal_status)?;
            parcel.write(&service_specific_status)?;
            assert_eq!(parcel.write(&failed_status).is_err(), true);
        }

        {
            assert_eq!(parcel.read::<Result<()>>()?, ok);
            assert_eq!(parcel.read::<Result<()>>()?, illegal_status);
            assert_eq!(parcel.read::<Result<()>>()?, service_specific_status);
            assert_eq!(parcel.read::<Result<()>>().is_err(), true);
        }

        Ok(())
    }
}
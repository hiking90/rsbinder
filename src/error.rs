use std::string::FromUtf16Error;
use std::array::TryFromSliceError;
use std::fmt;
use std::io;
use libc;

use crate::parcelable::*;
use crate::parcel::*;

pub type Result<T> = std::result::Result<T, Error>;

const UNKNOWN_ERROR: isize = -2147483647-1;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ErrorKind {
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
}

#[derive(Clone, Copy, Debug)]
pub enum Exception {
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


#[derive(Debug)]
enum Inner {
    Exception(i32, Exception, Option<String>),
    Any(Box<dyn std::error::Error>),
}

#[derive(Debug)]
pub struct Error {
    inner: Inner
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error {
            inner: Inner::Exception(kind as _, Exception::JustError, None)
        }
    }
}

impl From<i32> for Error {
    fn from(code: i32) -> Error {
        Error {
            inner: Inner::Exception(code, Exception::JustError, None)
        }
    }
}

impl From<(i32, Option<String>)> for Error {
    fn from(exception: (i32, Option<String>)) -> Error {
        Error {
            inner: Inner::Exception(exception.0, Exception::ServiceSpecific, exception.1)
        }
    }
}

impl From<(Exception, Option<String>)> for Error {
    fn from(exception: (Exception, Option<String>)) -> Error {
        Error {
            inner: Inner::Exception(0, exception.0, exception.1)
        }
    }
}

impl From<nix::errno::Errno> for Error {
    fn from(err: nix::errno::Errno) -> Error {
        Error {
            inner: Inner::Exception(err as _, Exception::JustError, None)
        }
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error {
            inner: Inner::Any(Box::new(err))
        }
    }
}

impl From<TryFromSliceError> for Error {
    fn from(err: TryFromSliceError) -> Error {
        Error {
            inner: Inner::Any(Box::new(err))
        }
    }
}

impl From<FromUtf16Error> for Error {
    fn from(err: FromUtf16Error) -> Error {
        Error {
            inner: Inner::Any(Box::new(err))
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.inner {
            Inner::Exception(code, exception, message) => {
                write!(fmt, "rsbinder::Error Exception {:?}, Code: {}, Message: {:?}", exception, code, message)
            }
            Inner::Any(ref e) => e.fmt(fmt),
        }
    }
}

#[derive(PartialEq, Debug)]
pub struct Status {
    code: i32,
    exception: i32,
    message: String,
}

impl Status {
    fn new(code: i32, exception: i32, message: String) -> Self {
        Status {
            code: code,
            exception: exception,
            message: message,
        }
    }
}

impl<T> From<Result<T>> for Status {
    fn from(result: Result<T>) -> Status {
        match result {
            Ok(_) => {
                Status {
                    code: 0,
                    exception: Exception::None as _,
                    message: "".to_string(),
                }
            }
            Err(err) => {
                match &err.inner {
                    Inner::Exception(code, exception, message) => {
                        Status {
                            code: *code,
                            exception: (*exception) as _,
                            message: if let Some(message) = message {
                                message.to_string()
                            } else {
                                "".to_string()
                            },
                        }

                    }
                    Inner::Any(ref e) => {
                        Status {
                            code: ErrorKind::Unknown as _,
                            exception: Exception::IllegalState as _,
                            message: format!("{:?}", e),
                        }
                    }
                }
            }
        }
    }
}


impl Serialize for Status {
    fn serialize(&self, parcel: &mut WritableParcel<'_>) -> Result<()> {
        let exception = self.exception;
        if exception == Exception::TransactionFailed as i32 {
            return Err(Error::from((Exception::TransactionFailed, Some(self.message.clone()))))
        }

        parcel.write::<i32>(&exception)?;
        if exception == Exception::None as i32 {
            return Ok(())
        }

        parcel.write(&String16(self.message.clone()))?;
        parcel.write::<i32>(&0)?;
        if exception == Exception::ServiceSpecific as i32 {
            // There are no usecases in Android. So, it just set 0.
            parcel.write::<i32>(&self.code)?;
        } else {
            parcel.write::<i32>(&0)?;
        }

        Ok(())
    }
}

impl Deserialize for Status {
    fn deserialize(parcel: &mut ReadableParcel<'_>) -> Result<Self> {
        let exception = parcel.read::<i32>()?;

        let status = if exception == Exception::None as i32 {
            Status {
                code: 0,
                exception: exception,
                message: "".to_string(),
            }
        } else {
            let message = parcel.read::<String16>()?;
            _ = parcel.read::<i32>()?;
            let code = parcel.read::<i32>()?;

            Status {
                code: code,
                exception: exception,
                message: message.0,
            }
        };

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_status_parcelable() -> Result<()> {
        let ok = Status::new(0, Exception::None as _, "".to_string());
        let illegal_status = Status::new(0, Exception::IllegalArgument as _, "IllegalArgument".to_string());
        let failed_status = Status::new(0, Exception::TransactionFailed as _, "TransactionFailed".to_string());
        let service_specific_status = Status::new(ErrorKind::NameNotFound as _, Exception::ServiceSpecific as _, "IllegalArgument".to_string());

        let mut parcel = Parcel::new();

        {
            let mut writer = parcel.as_writable();
            writer.write(&ok)?;
            writer.write(&illegal_status)?;
            writer.write(&service_specific_status)?;
            assert_eq!(writer.write(&failed_status).is_err(), true);
        }

        {
            let mut reader = parcel.as_readable();
            assert_eq!(reader.read::<Status>()?, ok);
            assert_eq!(reader.read::<Status>()?, illegal_status);
            assert_eq!(reader.read::<Status>()?, service_specific_status);
            assert_eq!(reader.read::<Status>().is_err(), true);
        }

        Ok(())
    }
}
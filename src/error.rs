use std::string::FromUtf16Error;
use std::array::TryFromSliceError;
use std::fmt;
use std::io;
use libc;

use crate::parcelable::*;
use crate::parcel::*;

pub type Result<T> = std::result::Result<T, Error>;
pub type Status<T> = std::result::Result<T, Exception>;


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
    ServiceSpecific = -8,
}



#[derive(Debug)]
pub enum Error {
    ErrorKind(i32),
    Any(Box<dyn std::error::Error>),
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error::ErrorKind(kind as _)
    }
}

impl From<i32> for Error {
    fn from(code: i32) -> Error {
        Error::ErrorKind(code as _)
    }
}

// impl From<(i32, Option<String>)> for Error {
//     fn from(exception: (i32, Option<String>)) -> Error {
//         Error {
//             inner: Inner::Exception(exception.0, ExceptionKind::ServiceSpecific, exception.1)
//         }
//     }
// }

// impl From<(Exception, Option<String>)> for Error {
//     fn from(exception: (Exception, Option<String>)) -> Error {
//         Error {
//             inner: Inner::Exception(0, exception.0, exception.1)
//         }
//     }
// }

impl From<nix::errno::Errno> for Error {
    fn from(err: nix::errno::Errno) -> Error {
        Error::ErrorKind(err as _)
    }
}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Error {
        Error::Any(Box::new(err))
    }
}

impl From<TryFromSliceError> for Error {
    fn from(err: TryFromSliceError) -> Error {
        Error::Any(Box::new(err))
    }
}

impl From<FromUtf16Error> for Error {
    fn from(err: FromUtf16Error) -> Error {
        Error::Any(Box::new(err))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::ErrorKind(error) => {
                write!(fmt, "rsbinder::ErrorKind {}", error)
            }
            // Inner::Exception(code, exception, message) => {
            //     write!(fmt, "rsbinder::Error Exception {:?}, Code: {}, Message: {:?}", exception, code, message)
            // }
            Error::Any(ref e) => e.fmt(fmt),
        }
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
    code: i32,
    exception: i32,
    message: String,
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

impl From<ExceptionKind> for Exception {
    fn from(exception: ExceptionKind) -> Self {
        Exception {
            code: 0,
            exception: exception as _,
            message: format!("ExceptionKind: {:?}", exception),
        }
    }
}

impl From<Error> for Exception {
    fn from(error: Error) -> Exception {
        match error {
            Error::ErrorKind(code) => {
                Exception {
                    code: code,
                    exception: ErrorKind::ServiceSpecific as _,
                    message: format!("ErrorKind {}", code),
                }

            }
            Error::Any(ref e) => {
                Exception {
                    code: ErrorKind::Unknown as _,
                    exception: ExceptionKind::IllegalState as _,
                    message: format!("{:?}", e),
                }
            }
        }
    }
}


impl<T> Serialize for Status<T> {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        match self {
            Ok(_) => {
                parcel.write::<i32>(&0)?;
            }
            Err(err) => {
                let exception = err.exception;
                if exception == ExceptionKind::TransactionFailed as i32 {
                    return Err(Error::from(ErrorKind::FailedTransaction))
                }

                parcel.write::<i32>(&exception)?;
                if exception == ExceptionKind::None as i32 {
                    return Ok(())
                }

                parcel.write(&String16(err.message.clone()))?;
                parcel.write::<i32>(&0)?;
                if exception == ExceptionKind::ServiceSpecific as i32 {
                    // There are no usecases in Android. So, it just set 0.
                    parcel.write::<i32>(&err.code)?;
                } else {
                    parcel.write::<i32>(&0)?;
                }

            }
        }

        Ok(())
    }
}

impl Deserialize for Status<()> {
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
            })
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
        let illegal_status = Status::<()>::Err(
            Exception::new(0, ExceptionKind::IllegalArgument as _, "IllegalArgument".to_string()));
        let failed_status = Status::<()>::Err(
            Exception::new(0, ExceptionKind::TransactionFailed as _, "TransactionFailed".to_string()));
        let service_specific_status = Status::<()>::Err(
            Exception::new(ErrorKind::NameNotFound as _,
                ExceptionKind::ServiceSpecific as _, "IllegalArgument".to_string()));

        let mut parcel = Parcel::new();

        {
            parcel.write(&ok)?;
            parcel.write(&illegal_status)?;
            parcel.write(&service_specific_status)?;
            assert_eq!(parcel.write(&failed_status).is_err(), true);
        }

        {
            assert_eq!(parcel.read::<Status<()>>()?, ok);
            assert_eq!(parcel.read::<Status<()>>()?, illegal_status);
            assert_eq!(parcel.read::<Status<()>>()?, service_specific_status);
            assert_eq!(parcel.read::<Status<()>>().is_err(), true);
        }

        Ok(())
    }
}
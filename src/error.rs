use std::array::TryFromSliceError;
use std::fmt;
use std::io;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[non_exhaustive]
pub enum ErrorKind {
    Unknown,
    InvalidOperation,
    BadType,
    BadFd,
    NameNotFound,
    NoInit,
    DeadObject,
    FailedTransaction,
    UnknownTransaction,
    BadIndex,
    FdsNotAllowed,
    UnexpectedNull,
    NotEnoughData,
    Other(i32),
}

#[derive(Debug)]
enum Inner {
    ErrorKind(ErrorKind),
    Any(Box<dyn std::error::Error>),
}

#[derive(Debug)]
pub struct Error {
    inner: Inner
}

impl From<ErrorKind> for Error {
    fn from(kind: ErrorKind) -> Error {
        Error {
            inner: Inner::ErrorKind(kind)
        }
    }
}

impl From<nix::errno::Errno> for Error {
    fn from(err: nix::errno::Errno) -> Error {
        Error {
            inner: Inner::ErrorKind(ErrorKind::Other(err as _))
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

impl fmt::Display for Error {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.inner {
            Inner::ErrorKind(kind) => {
                write!(fmt, "rsbinder::Error Kind{:?} ", kind)
            }
            Inner::Any(ref e) => e.fmt(fmt),
        }
    }
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Status and exception handling for binder operations.
//!
//! This module provides status types for handling exceptions and errors
//! that occur during binder transactions, including both system-level
//! errors and application-specific exceptions.

use crate::error;
use crate::error::StatusCode;
use crate::parcel::*;
use crate::parcelable::*;
use std::fmt::{Debug, Display, Formatter};

/// Result type for operations that can return a `Status` error.
pub type Result<T> = std::result::Result<T, Status>;

/// Exception codes for binder operations.
///
/// These codes represent different types of exceptions that can occur
/// during binder transactions, corresponding to Java exception types
/// in the Android framework.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(i32)]
pub enum ExceptionCode {
    /// No exception occurred
    None = 0,
    /// Security exception
    Security = -1,
    /// Bad parcelable data
    BadParcelable = -2,
    /// Illegal argument provided
    IllegalArgument = -3,
    /// Null pointer exception
    NullPointer = -4,
    /// Illegal state exception
    IllegalState = -5,
    /// Network operation on main thread
    NetworkMainThread = -6,
    /// Unsupported operation
    UnsupportedOperation = -7,
    /// Service-specific exception
    ServiceSpecific = -8,
    /// Parcelable exception
    Parcelable = -9,

    // This is special and Java specific; see Parcel.java.
    /// Has reply header (Java-specific)
    HasReplyHeader = -128,
    // This is special, and indicates to C++ binder proxies that the
    // transaction has failed at a low level.
    /// Transaction failed at low level
    TransactionFailed = -129,
    /// Generic error
    JustError = -256,
}

impl Display for ExceptionCode {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            ExceptionCode::None => write!(f, "None"),
            ExceptionCode::Security => write!(f, "Security"),
            ExceptionCode::BadParcelable => write!(f, "BadParcelable"),
            ExceptionCode::IllegalArgument => write!(f, "IllegalArgument"),
            ExceptionCode::NullPointer => write!(f, "NullPointer"),
            ExceptionCode::IllegalState => write!(f, "IllegalState"),
            ExceptionCode::NetworkMainThread => write!(f, "NetworkMainThread"),
            ExceptionCode::UnsupportedOperation => write!(f, "UnsupportedOperation"),
            ExceptionCode::ServiceSpecific => write!(f, "ServiceSpecific"),
            ExceptionCode::Parcelable => write!(f, "Parcelable"),
            ExceptionCode::HasReplyHeader => write!(f, "HasReplyHeader"),
            ExceptionCode::TransactionFailed => write!(f, "TransactionFailed"),
            ExceptionCode::JustError => write!(f, "JustError"),
        }
    }
}

impl Serialize for ExceptionCode {
    fn serialize(&self, parcel: &mut Parcel) -> error::Result<()> {
        parcel.write::<i32>(&(*self as i32))
    }
}

impl Deserialize for ExceptionCode {
    fn deserialize(parcel: &mut Parcel) -> error::Result<Self> {
        let exception = parcel.read::<i32>()?;
        let code = match exception {
            exception if exception == ExceptionCode::None as i32 => ExceptionCode::None,
            exception if exception == ExceptionCode::Security as i32 => ExceptionCode::Security,
            exception if exception == ExceptionCode::BadParcelable as i32 => {
                ExceptionCode::BadParcelable
            }
            exception if exception == ExceptionCode::IllegalArgument as i32 => {
                ExceptionCode::IllegalArgument
            }
            exception if exception == ExceptionCode::NullPointer as i32 => {
                ExceptionCode::NullPointer
            }
            exception if exception == ExceptionCode::IllegalState as i32 => {
                ExceptionCode::IllegalState
            }
            exception if exception == ExceptionCode::NetworkMainThread as i32 => {
                ExceptionCode::NetworkMainThread
            }
            exception if exception == ExceptionCode::UnsupportedOperation as i32 => {
                ExceptionCode::UnsupportedOperation
            }
            exception if exception == ExceptionCode::ServiceSpecific as i32 => {
                ExceptionCode::ServiceSpecific
            }
            exception if exception == ExceptionCode::Parcelable as i32 => ExceptionCode::Parcelable,
            exception if exception == ExceptionCode::HasReplyHeader as i32 => {
                ExceptionCode::HasReplyHeader
            }
            exception if exception == ExceptionCode::TransactionFailed as i32 => {
                ExceptionCode::TransactionFailed
            }
            _ => ExceptionCode::JustError,
        };
        Ok(code)
    }
}

/// Status information for binder operations.
///
/// `Status` combines an exception code, status code, and optional message
/// to provide comprehensive error information for binder transactions.
/// It can represent both successful operations and various failure modes.
pub struct Status {
    code: StatusCode,
    exception: ExceptionCode,
    message: Option<String>,
}

impl PartialEq for Status {
    fn eq(&self, other: &Self) -> bool {
        self.code == other.code && self.exception == other.exception
    }
}

impl Status {
    fn new(exception: ExceptionCode, status: StatusCode, message: Option<String>) -> Self {
        Status {
            code: status,
            exception,
            message,
        }
    }

    pub fn new_service_specific_error(err: i32, message: Option<String>) -> Self {
        Self::new(
            ExceptionCode::ServiceSpecific,
            StatusCode::ServiceSpecific(err),
            message,
        )
    }

    pub fn is_ok(&self) -> bool {
        self.exception == ExceptionCode::None
    }

    pub fn exception_code(&self) -> ExceptionCode {
        self.exception
    }

    pub fn transaction_error(&self) -> StatusCode {
        if self.exception == ExceptionCode::TransactionFailed {
            self.code
        } else {
            StatusCode::Ok
        }
    }

    pub fn service_specific_error(&self) -> i32 {
        if let StatusCode::ServiceSpecific(err) = self.code {
            err
        } else {
            0
        }
    }
}

impl std::error::Error for Status {}

impl Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.exception == ExceptionCode::None {
            write!(f, "{}", self.code)
        } else {
            write!(
                f,
                "{} / {}: {}",
                self.exception,
                self.code,
                self.message.as_ref().unwrap_or(&"".to_owned())
            )
        }
    }
}

impl Debug for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        Display::fmt(self, f)
    }
}

impl From<ExceptionCode> for StatusCode {
    fn from(exception: ExceptionCode) -> Self {
        match exception {
            ExceptionCode::TransactionFailed => StatusCode::FailedTransaction,
            _ => StatusCode::Ok,
        }
    }
}

impl From<Status> for StatusCode {
    fn from(status: Status) -> Self {
        status.code
    }
}

impl From<StatusCode> for ExceptionCode {
    fn from(status: StatusCode) -> Self {
        match status {
            StatusCode::Ok => ExceptionCode::None,
            StatusCode::UnexpectedNull => ExceptionCode::NullPointer,
            StatusCode::ServiceSpecific(_) => ExceptionCode::ServiceSpecific,
            _ => ExceptionCode::TransactionFailed,
        }
    }
}

impl From<ExceptionCode> for Status {
    fn from(exception: ExceptionCode) -> Self {
        Status::new(exception, exception.into(), None)
    }
}

impl From<(ExceptionCode, &str)> for Status {
    fn from(arg: (ExceptionCode, &str)) -> Self {
        Status::new(arg.0, arg.0.into(), Some(arg.1.to_owned()))
    }
}

impl From<StatusCode> for Status {
    fn from(status: StatusCode) -> Self {
        Status::new(status.into(), status, None)
    }
}

impl Serialize for Status {
    fn serialize(&self, parcel: &mut Parcel) -> error::Result<()> {
        if self.exception == ExceptionCode::TransactionFailed {
            return Err(self.code);
        }

        parcel.write::<i32>(&(self.exception as _))?;
        if self.exception == ExceptionCode::None {
            return Ok(());
        }

        parcel.write::<String>(self.message.as_ref().unwrap_or(&"".to_owned()))?;
        parcel.write::<i32>(&0)?; // Empty remote stack trace header

        if self.exception == ExceptionCode::ServiceSpecific {
            parcel.write::<i32>(&(self.code.into()))?;
        } else if self.exception == ExceptionCode::Parcelable {
            parcel.write::<i32>(&0)?;
        }

        Ok(())
    }
}

fn read_check_header_size(parcel: &mut Parcel) -> error::Result<()> {
    // Skip over the blob of Parcelable data
    let header_start = parcel.data_position();
    // Get available size before reading more
    let header_avail = parcel.data_avail();

    let header_size = parcel.read::<i32>()?;

    if header_size < 0 || header_size as usize > header_avail {
        log::error!("0x534e4554:132650049 Invalid header_size({}).", header_size);
        return Err(StatusCode::Unknown);
    }
    parcel.set_data_position(header_start + (header_size as usize));
    Ok(())
}

impl Deserialize for Status {
    fn deserialize(parcel: &mut Parcel) -> error::Result<Self> {
        let mut exception = parcel.read::<ExceptionCode>()?;

        if exception == ExceptionCode::HasReplyHeader {
            read_check_header_size(parcel)?;
            exception = ExceptionCode::None;
        }
        let status = if exception == ExceptionCode::None {
            exception.into()
        } else {
            let message: String = parcel.read::<String>()?;
            let remote_stack_trace_header_size = parcel.read::<i32>()?;
            if remote_stack_trace_header_size < 0
                || remote_stack_trace_header_size as usize > parcel.data_avail()
            {
                log::error!(
                    "0x534e4554:132650049 Invalid remote_stack_trace_header_size({}).",
                    remote_stack_trace_header_size
                );
                return Err(StatusCode::Unknown);
            }
            parcel.set_data_position(
                parcel.data_position() + remote_stack_trace_header_size as usize,
            );

            let code = if exception == ExceptionCode::ServiceSpecific {
                let code = parcel.read::<i32>()?;
                StatusCode::ServiceSpecific(code)
            } else if exception == ExceptionCode::Parcelable {
                read_check_header_size(parcel)?;
                StatusCode::Ok
            } else {
                StatusCode::Ok
            };

            Status::new(exception, code, Some(message))
        };

        Ok(status)
    }
}

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn test_status() -> Result<()> {
        let _status = Status::from(StatusCode::Unknown);

        Ok(())
    }

    #[test]
    fn test_status_display() -> Result<()> {
        let unknown = Status::from(StatusCode::Unknown);
        assert_eq!(format!("{}", unknown), "TransactionFailed / Unknown: ");

        let service_specific =
            Status::new_service_specific_error(1, Some("Service specific error".to_owned()));
        assert_eq!(
            format!("{}", service_specific),
            "ServiceSpecific / ServiceSpecific(1): Service specific error"
        );

        let exception = Status::new(
            ExceptionCode::BadParcelable,
            StatusCode::Unknown,
            Some("Bad parcelable".to_owned()),
        );
        assert_eq!(
            format!("{}", exception),
            "BadParcelable / Unknown: Bad parcelable"
        );

        Ok(())
    }

    #[test]
    fn test_status_serialize() -> Result<()> {
        let status = Status::from(StatusCode::ServiceSpecific(1));
        let mut parcel = Parcel::new();
        status.serialize(&mut parcel).unwrap();

        // deserialize
        parcel.set_data_position(0);
        let deserialized = Status::deserialize(&mut parcel).unwrap();
        assert_eq!(status, deserialized);

        // serialize parcelable
        let status = Status::new(
            ExceptionCode::Parcelable,
            StatusCode::Ok,
            Some("Parcelable".to_owned()),
        );
        let mut parcel = Parcel::new();
        status.serialize(&mut parcel).unwrap();

        // deserialize parcelable
        parcel.set_data_position(0);
        let deserialized = Status::deserialize(&mut parcel).unwrap();
        assert_eq!(status, deserialized);

        Ok(())
    }
}

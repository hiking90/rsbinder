// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use crate::parcelable::*;
use crate::parcel::*;
use crate::error::*;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
#[repr(i32)]
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

impl Serialize for ExceptionCode {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write::<i32>(&(*self as i32))
    }
}

impl Deserialize for ExceptionCode {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let exception = parcel.read::<i32>()?;
        let code = match exception {
            exception if exception == ExceptionCode::None as i32 => ExceptionCode::None,
            exception if exception == ExceptionCode::Security as i32 => ExceptionCode::Security,
            exception if exception == ExceptionCode::BadParcelable as i32 => ExceptionCode::BadParcelable,
            exception if exception == ExceptionCode::IllegalArgument as i32 => ExceptionCode::IllegalArgument,
            exception if exception == ExceptionCode::NullPointer as i32 => ExceptionCode::NullPointer,
            exception if exception == ExceptionCode::IllegalState as i32 => ExceptionCode::IllegalState,
            exception if exception == ExceptionCode::NetworkMainThread as i32 => ExceptionCode::NetworkMainThread,
            exception if exception == ExceptionCode::UnsupportedOperation as i32 => ExceptionCode::UnsupportedOperation,
            exception if exception == ExceptionCode::ServiceSpecific as i32 => ExceptionCode::ServiceSpecific,
            exception if exception == ExceptionCode::Parcelable as i32 => ExceptionCode::Parcelable,
            exception if exception == ExceptionCode::HasReplyHeader as i32 => ExceptionCode::HasReplyHeader,
            exception if exception == ExceptionCode::TransactionFailed as i32 => ExceptionCode::TransactionFailed,
            _ => ExceptionCode::JustError,
        };
        Ok(code)
    }
}


pub struct Status {
    code: StatusCode,
    exception: ExceptionCode,
    message: Option<String>,
}

impl Status {
    fn new(exception: ExceptionCode, status: StatusCode, message: Option<String>) -> Self {
        Status {
            code: status,
            exception,
            message,
        }
    }

    pub fn service_specific_error(status: StatusCode, message: Option<String>) -> Self {
        Self::new(ExceptionCode::ServiceSpecific, status, message)
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

impl From<StatusCode> for ExceptionCode {
    fn from(status: StatusCode) -> Self {
        match status {
            StatusCode::Ok => ExceptionCode::None,
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

impl Serialize for Status {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        if self.exception == ExceptionCode::TransactionFailed {
            return Err(self.code)
        }

        parcel.write::<i32>(&(self.exception as _))?;
        if self.exception == ExceptionCode::None {
            return Ok(())
        }

        parcel.write(&self.message)?;
        parcel.write::<i32>(&0)?; // Empty remote stack trace header

        if self.exception == ExceptionCode::ServiceSpecific {
            parcel.write::<i32>(&(self.code.into()))?;
        } else if self.exception == ExceptionCode::Parcelable {
            parcel.write::<i32>(&0)?;
        }

        Ok(())
    }
}

fn read_check_header_size(parcel: &mut Parcel) -> Result<()> {
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
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let mut exception = parcel.read::<ExceptionCode>()?;

        if exception == ExceptionCode::HasReplyHeader {
            read_check_header_size(parcel)?;
            exception = ExceptionCode::None;
        }

        let status = if exception == ExceptionCode::None {
            exception.into()
        } else {
            let message = parcel.read::<String>()?;
            let remote_stack_trace_header_size = parcel.read::<i32>()?;
            if remote_stack_trace_header_size < 0 || remote_stack_trace_header_size as usize > parcel.data_avail() {
                log::error!("0x534e4554:132650049 Invalid remote_stack_trace_header_size({}).", remote_stack_trace_header_size);
                return Err(StatusCode::Unknown);
            }
            parcel.set_data_position(parcel.data_position() + remote_stack_trace_header_size as usize);

            let code = if exception == ExceptionCode::ServiceSpecific {
                parcel.read::<i32>()?.into()
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

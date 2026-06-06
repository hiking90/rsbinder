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

    /// Reply parcel carries a noted-AppOps blob before the *real*
    /// exception code. Wire value `-127` matches
    /// AOSP `EX_HAS_NOTED_APPOPS_REPLY_HEADER`
    /// (`frameworks/native/libs/binder/include/binder/Status.h:71`).
    ///
    /// Set by a server that received the transaction with
    /// `TF_COLLECT_NOTED_APP_OPS` (see [`crate::FLAG_COLLECT_NOTED_APP_OPS`]).
    /// rsbinder's [`Status::deserialize`] transparently skips the header
    /// and reads the next i32 as the actual exception code — clients
    /// observe the same `Status` they would have without the header.
    HasNotedAppOpsReplyHeader = -127,
    // This is special and Java specific; see Parcel.java.
    /// Has reply header (Java-specific). Wire value `-128` matches AOSP
    /// `EX_HAS_REPLY_HEADER`. Treated as `EX_NONE` after the header is
    /// skipped — the AOSP convention for "fat response header"
    /// piggybacking.
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
            ExceptionCode::HasNotedAppOpsReplyHeader => write!(f, "HasNotedAppOpsReplyHeader"),
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
            exception if exception == ExceptionCode::HasNotedAppOpsReplyHeader as i32 => {
                ExceptionCode::HasNotedAppOpsReplyHeader
            }
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
///
/// `Clone` is derived (every field is `Clone`) so callers can fan a
/// failed result out to multiple handlers, copy it while logging, or
/// re-surface it on retry — mirroring AOSP's copyable `Status`. `PartialEq`
/// is a manual impl (it deliberately ignores `message`), so it is not part
/// of the derive.
#[derive(Clone)]
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
        // A Status that carries an exception must never collapse to `Ok`.
        // An application-level exception (e.g. IllegalArgument) with an
        // `Ok` status code means "no transaction error, but the call did
        // not succeed" — surface it as FailedTransaction so callers that
        // map this into a `Result` cannot end up with `Err(StatusCode::Ok)`.
        // The success path (exception None) and real error codes
        // (TransactionFailed / ServiceSpecific) are preserved unchanged.
        match status.code {
            StatusCode::Ok if status.exception != ExceptionCode::None => {
                StatusCode::FailedTransaction
            }
            code => code,
        }
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
        // Mirrors AOSP `Status::writeToParcel`: on EX_TRANSACTION_FAILED
        // the binder layer already failed, so nothing is written and the
        // status code is returned via the error channel rather than as
        // wire data. This is intentionally indistinguishable from a real
        // parcel-write failure — same as libbinder ("not going to even
        // try returning rich error data").
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

    // Check for negative values first
    if header_size < 0 {
        log::error!("0x534e4554:132650049 Negative header_size({header_size}).");
        return Err(StatusCode::BadValue);
    }

    // Safe conversion after negativity check
    let header_size_usize = header_size as usize;

    // Check against available data
    if header_size_usize > header_avail {
        log::error!(
            "0x534e4554:132650049 Invalid header_size({header_size}) exceeds available({header_avail})."
        );
        return Err(StatusCode::BadValue);
    }

    // Prevent integer overflow in position calculation
    let new_position = header_start.checked_add(header_size_usize).ok_or_else(|| {
        log::error!("0x534e4554:132650049 Position overflow with header_size({header_size})");
        StatusCode::BadValue
    })?;

    parcel.set_data_position(new_position);
    Ok(())
}

impl Deserialize for Status {
    fn deserialize(parcel: &mut Parcel) -> error::Result<Self> {
        let mut exception = parcel.read::<ExceptionCode>()?;

        // AOSP-faithful order — `Status::readFromParcel` in
        // `frameworks/native/libs/binder/Status.cpp`:
        //   1. EX_HAS_NOTED_APPOPS_REPLY_HEADER → skip blob, re-read
        //      the next i32 as the actual exception code (which may
        //      itself be EX_HAS_REPLY_HEADER).
        //   2. EX_HAS_REPLY_HEADER → skip blob, treat as EX_NONE
        //      (libbinder convention for "fat response header").
        if exception == ExceptionCode::HasNotedAppOpsReplyHeader {
            read_check_header_size(parcel)?;
            exception = parcel.read::<ExceptionCode>()?;
        }
        if exception == ExceptionCode::HasReplyHeader {
            read_check_header_size(parcel)?;
            exception = ExceptionCode::None;
        }
        let status = if exception == ExceptionCode::None {
            exception.into()
        } else {
            let message: String = parcel.read::<String>()?;

            // AOSP `Status::readFromParcel` (frameworks/native/libs/binder/
            // Status.cpp): capture the header start position and the
            // available bytes BEFORE reading the size int32. The remote
            // stack-trace header size is size-INCLUSIVE (it counts the
            // 4-byte size field itself), so reposition to
            // `header_start + size` — and ONLY when size != 0. size == 0
            // (the native writer's "empty remote stack trace header") leaves
            // the cursor right after the size field. The previous
            // size-EXCLUSIVE arithmetic (`current_pos_after_read + size`)
            // landed 4 bytes too far whenever a Java peer propagated a
            // non-zero stack-trace header.
            let header_start = parcel.data_position();
            let header_avail = parcel.data_avail();
            let remote_stack_trace_header_size = parcel.read::<i32>()?;

            // Check for negative values first
            if remote_stack_trace_header_size < 0 {
                log::error!(
                    "0x534e4554:132650049 Negative remote_stack_trace_header_size({remote_stack_trace_header_size})."
                );
                return Err(StatusCode::BadValue);
            }

            // Safe conversion after negativity check
            let trace_size_usize = remote_stack_trace_header_size as usize;

            // Check against available data (pre-read avail, which includes
            // the 4-byte size field, mirroring AOSP's `remote_avail`).
            if trace_size_usize > header_avail {
                log::error!(
                    "0x534e4554:132650049 Invalid remote_stack_trace_header_size({remote_stack_trace_header_size}) exceeds available({header_avail})."
                );
                return Err(StatusCode::BadValue);
            }

            if trace_size_usize != 0 {
                // Prevent integer overflow in position calculation
                let new_position = header_start.checked_add(trace_size_usize).ok_or_else(|| {
                    log::error!(
                        "0x534e4554:132650049 Position overflow with remote_stack_trace_header_size({remote_stack_trace_header_size})"
                    );
                    StatusCode::BadValue
                })?;

                parcel.set_data_position(new_position);
            }

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
        assert_eq!(format!("{unknown}"), "TransactionFailed / Unknown: ");

        let service_specific =
            Status::new_service_specific_error(1, Some("Service specific error".to_owned()));
        assert_eq!(
            format!("{service_specific}"),
            "ServiceSpecific / ServiceSpecific(1): Service specific error"
        );

        let exception = Status::new(
            ExceptionCode::BadParcelable,
            StatusCode::Unknown,
            Some("Bad parcelable".to_owned()),
        );
        assert_eq!(
            format!("{exception}"),
            "BadParcelable / Unknown: Bad parcelable"
        );

        Ok(())
    }

    // Regression: a non-zero, size-INCLUSIVE remote stack-trace header
    // (as a Java peer propagating an exception trace emits) must be skipped
    // by exactly `header_start + size` so the following EX_SERVICE_SPECIFIC
    // code reads back correctly. The previous size-EXCLUSIVE arithmetic
    // (`pos_after_size_read + size`) overshot by 4 bytes and desynced the
    // cursor.
    #[test]
    fn deserialize_skips_nonzero_remote_stack_trace_header() {
        let mut parcel = Parcel::new();
        parcel
            .write::<i32>(&(ExceptionCode::ServiceSpecific as i32))
            .unwrap();
        parcel.write::<String>(&"boom".to_owned()).unwrap();
        // Size-inclusive header: 4 (the size field) + 4 (one i32 of opaque
        // trace payload) = 8.
        parcel.write::<i32>(&8i32).unwrap();
        parcel.write::<i32>(&0x5555_5555i32).unwrap(); // trace payload
        parcel.write::<i32>(&777i32).unwrap(); // service-specific code

        parcel.set_data_position(0);
        let status = Status::deserialize(&mut parcel).expect("deserialize");
        assert_eq!(
            StatusCode::from(status),
            StatusCode::ServiceSpecific(777),
            "non-zero size-inclusive stack-trace header must be skipped exactly"
        );
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

    // Regression: a Status that carries an application-level exception
    // but an `Ok` status code must never collapse to `StatusCode::Ok`.
    // Callers map this into a `Result`, so an `Err(StatusCode::Ok)`
    // would be a silent success that wasn't one.
    #[test]
    fn status_with_exception_never_maps_to_ok() {
        let status = Status::new(ExceptionCode::IllegalArgument, StatusCode::Ok, None);
        assert_eq!(
            StatusCode::from(status),
            StatusCode::FailedTransaction,
            "exception + Ok code must surface as FailedTransaction, not Ok"
        );

        // Success path: no exception, Ok code → preserved as Ok.
        let ok = Status::new(ExceptionCode::None, StatusCode::Ok, None);
        assert_eq!(StatusCode::from(ok), StatusCode::Ok);

        // Real error codes are preserved unchanged.
        let svc = Status::from(StatusCode::ServiceSpecific(7));
        assert_eq!(StatusCode::from(svc), StatusCode::ServiceSpecific(7));
        let txn = Status::new(
            ExceptionCode::TransactionFailed,
            StatusCode::DeadObject,
            None,
        );
        assert_eq!(StatusCode::from(txn), StatusCode::DeadObject);
    }

    // Regression: `Serialize for Status` mirrors AOSP
    // `Status::writeToParcel` — on EX_TRANSACTION_FAILED nothing is
    // written and the code is returned via the error channel instead
    // of as wire data.
    #[test]
    fn serialize_transaction_failed_returns_err_without_writing() {
        let status = Status::new(
            ExceptionCode::TransactionFailed,
            StatusCode::DeadObject,
            None,
        );
        let mut parcel = Parcel::new();
        assert_eq!(status.serialize(&mut parcel), Err(StatusCode::DeadObject));
        assert_eq!(
            parcel.data_size(),
            0,
            "nothing must be written on the failed path"
        );
    }

    // ----------------------------------------------------------------
    // EX_HAS_NOTED_APPOPS_REPLY_HEADER / EX_HAS_REPLY_HEADER
    // ----------------------------------------------------------------

    /// The bare wire values match AOSP `Status.h:71,74`.
    /// A driver running on a current Android release embeds these
    /// codes into reply parcels, so a mismatch would silently corrupt
    /// every reply that piggybacks a header.
    #[test]
    fn appops_header_exception_codes_match_aosp_wire() {
        assert_eq!(
            ExceptionCode::HasNotedAppOpsReplyHeader as i32,
            -127,
            "EX_HAS_NOTED_APPOPS_REPLY_HEADER must be -127 (AOSP Status.h:71)"
        );
        assert_eq!(
            ExceptionCode::HasReplyHeader as i32,
            -128,
            "EX_HAS_REPLY_HEADER must be -128 (AOSP Status.h:74)"
        );
    }

    /// Helper — write a fake length-prefixed header blob whose size
    /// field includes itself (AOSP convention,
    /// `Status.cpp::skipUnusedHeader`: "the header size includes the
    /// 4 byte size field"). The Parcel write path enforces 4-byte
    /// alignment, so each "payload byte" needs an `i32` slot; we round
    /// the requested byte count up to the nearest 4 before allocating
    /// the zero-filled slots.
    fn write_header_blob(parcel: &mut Parcel, header_payload_bytes: usize) {
        let aligned_payload = header_payload_bytes.div_ceil(4) * 4;
        let size_field = 4 + aligned_payload;
        parcel.write::<i32>(&(size_field as i32)).unwrap();
        for _ in 0..(aligned_payload / 4) {
            parcel.write::<i32>(&0i32).unwrap();
        }
    }

    /// A reply that starts with `EX_HAS_NOTED_APPOPS_REPLY_HEADER`
    /// (-127), then carries a blob, then the real `EX_NONE` (0), must
    /// decode as `Status::ok()` — the AppOps header is transparent to
    /// the user.
    #[test]
    fn deserialize_skips_appops_header_then_reads_ex_none() {
        let mut parcel = Parcel::new();
        parcel
            .write::<i32>(&(ExceptionCode::HasNotedAppOpsReplyHeader as i32))
            .unwrap();
        write_header_blob(&mut parcel, 16); // 16-byte fake AppOps blob
        parcel.write::<i32>(&(ExceptionCode::None as i32)).unwrap();

        parcel.set_data_position(0);
        let status = Status::deserialize(&mut parcel).expect("deserialize");
        assert_eq!(status.exception_code(), ExceptionCode::None);
        assert!(status.is_ok());
    }

    /// A reply that chains AppOps header → reply header
    /// (`Status.cpp::readFromParcel` allows the AppOps header first)
    /// must collapse to `EX_NONE` — the AOSP convention for "fat
    /// response header + no exception".
    #[test]
    fn deserialize_skips_appops_then_reply_header_collapses_to_none() {
        let mut parcel = Parcel::new();
        parcel
            .write::<i32>(&(ExceptionCode::HasNotedAppOpsReplyHeader as i32))
            .unwrap();
        write_header_blob(&mut parcel, 8);
        parcel
            .write::<i32>(&(ExceptionCode::HasReplyHeader as i32))
            .unwrap();
        write_header_blob(&mut parcel, 4);

        parcel.set_data_position(0);
        let status = Status::deserialize(&mut parcel).expect("deserialize");
        assert_eq!(
            status.exception_code(),
            ExceptionCode::None,
            "EX_HAS_REPLY_HEADER (-128) must collapse to None"
        );
    }

    /// AppOps header + real `EX_SECURITY` (-1) must surface
    /// the security exception to the caller — the header is
    /// transparent but the underlying error is not.
    #[test]
    fn deserialize_skips_appops_header_then_surfaces_real_exception() {
        let mut parcel = Parcel::new();
        parcel
            .write::<i32>(&(ExceptionCode::HasNotedAppOpsReplyHeader as i32))
            .unwrap();
        write_header_blob(&mut parcel, 12);
        // Real exception payload: code + message + trace size.
        parcel
            .write::<i32>(&(ExceptionCode::Security as i32))
            .unwrap();
        parcel.write::<String>(&"denied".to_owned()).unwrap();
        parcel.write::<i32>(&0i32).unwrap(); // remote stack trace header size

        parcel.set_data_position(0);
        let status = Status::deserialize(&mut parcel).expect("deserialize");
        assert_eq!(status.exception_code(), ExceptionCode::Security);
    }

    // ----------------------------------------------------------------
    // TF_UPDATE_TXN / TF_COLLECT_NOTED_APP_OPS flag values
    // ----------------------------------------------------------------

    /// `FLAG_UPDATE_TXN` must equal AOSP `TF_UPDATE_TXN = 0x40`
    /// (kernel UAPI `binder.h:346`). The kernel driver only checks the
    /// bit value, so a mismatch is a silent wire incompatibility.
    #[test]
    fn flag_update_txn_matches_kernel_wire() {
        assert_eq!(
            crate::FLAG_UPDATE_TXN,
            0x40,
            "FLAG_UPDATE_TXN must be the kernel-defined 0x40"
        );
    }

    /// `FLAG_COLLECT_NOTED_APP_OPS = 0x80` matches AOSP's
    /// userspace libbinder convention.
    #[test]
    fn flag_collect_noted_app_ops_matches_aosp_userspace() {
        assert_eq!(
            crate::FLAG_COLLECT_NOTED_APP_OPS,
            0x80,
            "FLAG_COLLECT_NOTED_APP_OPS must be 0x80 (AOSP userspace)"
        );
    }

    // ----------------------------------------------------------------
    // binder_extended_error struct layout
    // ----------------------------------------------------------------

    /// rsbinder's `ExtendedError` mirror must match the
    /// 12-byte kernel struct exactly — id (4) + command (4) + param (4).
    /// Any field reorder would corrupt the ioctl read.
    #[test]
    fn extended_error_struct_layout_matches_kernel() {
        use std::mem::{align_of, size_of};
        type Ee = crate::sys::binder_extended_error;
        assert_eq!(size_of::<Ee>(), 12);
        assert_eq!(align_of::<Ee>(), 4);
    }
}

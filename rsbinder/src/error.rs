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

/// `(host, asm-generic)` for each errno named on Linux, Android and Apple (bionic `errno*.h`).
const ERRNO_NUMBERING: [(i32, i32); 87] = {
    use rustix::io::Errno as E;
    [
        (E::PERM.raw_os_error(), 1),
        (E::NOENT.raw_os_error(), 2),
        (E::SRCH.raw_os_error(), 3),
        (E::INTR.raw_os_error(), 4),
        (E::IO.raw_os_error(), 5),
        (E::NXIO.raw_os_error(), 6),
        (E::TOOBIG.raw_os_error(), 7),
        (E::NOEXEC.raw_os_error(), 8),
        (E::BADF.raw_os_error(), 9),
        (E::CHILD.raw_os_error(), 10),
        (E::AGAIN.raw_os_error(), 11),
        (E::NOMEM.raw_os_error(), 12),
        (E::ACCESS.raw_os_error(), 13),
        (E::FAULT.raw_os_error(), 14),
        (E::NOTBLK.raw_os_error(), 15),
        (E::BUSY.raw_os_error(), 16),
        (E::EXIST.raw_os_error(), 17),
        (E::XDEV.raw_os_error(), 18),
        (E::NODEV.raw_os_error(), 19),
        (E::NOTDIR.raw_os_error(), 20),
        (E::ISDIR.raw_os_error(), 21),
        (E::INVAL.raw_os_error(), 22),
        (E::NFILE.raw_os_error(), 23),
        (E::MFILE.raw_os_error(), 24),
        (E::NOTTY.raw_os_error(), 25),
        (E::TXTBSY.raw_os_error(), 26),
        (E::FBIG.raw_os_error(), 27),
        (E::NOSPC.raw_os_error(), 28),
        (E::SPIPE.raw_os_error(), 29),
        (E::ROFS.raw_os_error(), 30),
        (E::MLINK.raw_os_error(), 31),
        (E::PIPE.raw_os_error(), 32),
        (E::DOM.raw_os_error(), 33),
        (E::RANGE.raw_os_error(), 34),
        (E::DEADLK.raw_os_error(), 35),
        (E::NAMETOOLONG.raw_os_error(), 36),
        (E::NOLCK.raw_os_error(), 37),
        (E::NOSYS.raw_os_error(), 38),
        (E::NOTEMPTY.raw_os_error(), 39),
        (E::LOOP.raw_os_error(), 40),
        (E::NOMSG.raw_os_error(), 42),
        (E::IDRM.raw_os_error(), 43),
        (E::NOSTR.raw_os_error(), 60),
        (E::NODATA.raw_os_error(), 61),
        (E::TIME.raw_os_error(), 62),
        (E::NOSR.raw_os_error(), 63),
        (E::REMOTE.raw_os_error(), 66),
        (E::NOLINK.raw_os_error(), 67),
        (E::PROTO.raw_os_error(), 71),
        (E::MULTIHOP.raw_os_error(), 72),
        (E::BADMSG.raw_os_error(), 74),
        (E::OVERFLOW.raw_os_error(), 75),
        (E::ILSEQ.raw_os_error(), 84),
        (E::USERS.raw_os_error(), 87),
        (E::NOTSOCK.raw_os_error(), 88),
        (E::DESTADDRREQ.raw_os_error(), 89),
        (E::MSGSIZE.raw_os_error(), 90),
        (E::PROTOTYPE.raw_os_error(), 91),
        (E::NOPROTOOPT.raw_os_error(), 92),
        (E::PROTONOSUPPORT.raw_os_error(), 93),
        (E::SOCKTNOSUPPORT.raw_os_error(), 94),
        (E::OPNOTSUPP.raw_os_error(), 95),
        (E::PFNOSUPPORT.raw_os_error(), 96),
        (E::AFNOSUPPORT.raw_os_error(), 97),
        (E::ADDRINUSE.raw_os_error(), 98),
        (E::ADDRNOTAVAIL.raw_os_error(), 99),
        (E::NETDOWN.raw_os_error(), 100),
        (E::NETUNREACH.raw_os_error(), 101),
        (E::NETRESET.raw_os_error(), 102),
        (E::CONNABORTED.raw_os_error(), 103),
        (E::CONNRESET.raw_os_error(), 104),
        (E::NOBUFS.raw_os_error(), 105),
        (E::ISCONN.raw_os_error(), 106),
        (E::NOTCONN.raw_os_error(), 107),
        (E::SHUTDOWN.raw_os_error(), 108),
        (E::TOOMANYREFS.raw_os_error(), 109),
        (E::TIMEDOUT.raw_os_error(), 110),
        (E::CONNREFUSED.raw_os_error(), 111),
        (E::HOSTDOWN.raw_os_error(), 112),
        (E::HOSTUNREACH.raw_os_error(), 113),
        (E::ALREADY.raw_os_error(), 114),
        (E::INPROGRESS.raw_os_error(), 115),
        (E::STALE.raw_os_error(), 116),
        (E::DQUOT.raw_os_error(), 122),
        (E::CANCELED.raw_os_error(), 125),
        (E::OWNERDEAD.raw_os_error(), 130),
        (E::NOTRECOVERABLE.raw_os_error(), 131),
    ]
};

/// Whether every `(host, asm-generic)` pair agrees, i.e. the host numbers errno as the wire does.
const fn numbers_errno_as_asm_generic(pairs: &[(i32, i32)]) -> bool {
    let mut i = 0;
    while i < pairs.len() {
        if pairs[i].0 != pairs[i].1 {
            return false;
        }
        i += 1;
    }
    true
}

/// The host numbers errno unlike the wire (Linux asm-generic), so an unnamed errno cannot cross it.
const FOLD_UNNAMED_ERRNO: bool = !numbers_errno_as_asm_generic(&ERRNO_NUMBERING);

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
    /// RPC transport/protocol error (binder-over-socket stack).
    ///
    /// Payload-free on purpose: `StatusCode` derives `Copy`/`Ord`/`Hash`
    /// and has three hand-written exhaustive matches, so a rich payload
    /// variant is impossible here. The detailed error lives in
    /// [`crate::rpc::RpcError`]; this variant is only the boundary
    /// projection used when an RPC failure must surface through
    /// `rsbinder::Result`. Present only with the `rpc` feature; a build
    /// without it has no such variant.
    #[cfg(feature = "rpc")]
    RpcError,
    /// A negative status with no named variant.
    ///
    /// Built from an OS error by `From<rustix::io::Errno>` or
    /// `From<std::io::Error>`, the payload is the host's errno, negated, in
    /// the host's numbering (Darwin's on Apple platforms); an errno that has a
    /// named variant (`EPIPE` → [`StatusCode::DeadObject`], …) never becomes
    /// `Errno`. Decoded by `StatusCode::from(i32)`, the payload is the
    /// received `status_t` itself, which need not be an errno: AOSP
    /// `FROZEN_OBJECT` (`UNKNOWN_ERROR + 9`) arrives as
    /// `Errno(i32::MIN + 9)`.
    ///
    /// The payload crosses the wire unchanged only where the host numbers
    /// errno as the wire does, which is the Linux asm-generic numbering
    /// (bionic `asm-generic/errno*.h`, the values AOSP `utils/Errors.h` has
    /// on Android). The rule is decided at compile time by comparing the
    /// host's errno values with that numbering, not by the OS name:
    ///
    /// - A host with asm-generic numbering (Linux and Android on every
    ///   architecture except SPARC, MIPS, Alpha and PA-RISC) sends a negative
    ///   payload as is and decodes every unnamed negative status as `Errno`,
    ///   as AOSP C++ libbinder does.
    /// - On any other host (Apple platforms; Linux on SPARC, MIPS, Alpha or
    ///   PA-RISC) no unnamed status crosses: `Errno` is sent as
    ///   `UNKNOWN_ERROR` and an unnamed negative status decodes as
    ///   [`StatusCode::Unknown`], as AOSP's NDK (`PruneStatusT`) and Rust
    ///   (`parse_status_code`) backends do on every host.
    ///
    /// The payload must be negative. `Errno(x)` with `x >= 0` is sent as
    /// `UNKNOWN_ERROR` on every host: `0` would read as success and a
    /// positive value as [`StatusCode::ServiceSpecific`].
    Errno(i32),
    /// A positive status with no named variant, or the error code of a
    /// [`Status`](crate::Status) whose exception is
    /// [`ExceptionCode::ServiceSpecific`](crate::ExceptionCode::ServiceSpecific).
    ///
    /// As a transaction status (`i32::from`, the `Err` of a handler) only a
    /// positive payload crosses: `ServiceSpecific(v)` with `v <= 0` is sent
    /// as `UNKNOWN_ERROR`, since `0` would read as success and a negative
    /// value as a named variant or [`StatusCode::Errno`]. Inside a `Status`
    /// the payload is AOSP's `int32` service-specific error code and is
    /// written unchanged whatever its sign, as
    /// `Status::fromServiceSpecificError` does; that includes the `0` that
    /// `StatusCode::from(ExceptionCode::ServiceSpecific)` carries, AOSP's
    /// `Status::fromExceptionCode(EX_SERVICE_SPECIFIC)`.
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
            #[cfg(feature = "rpc")]
            StatusCode::RpcError => write!(f, "RpcError"),
            StatusCode::Errno(errno) => write!(f, "Errno({errno})"),
            StatusCode::ServiceSpecific(v) => write!(f, "ServiceSpecific({v})"),
        }
    }
}

/// One list drives both directions, so a named code cannot encode and decode differently.
macro_rules! wire_codes {
    ($($(#[$attr:meta])* $variant:ident = $name:ident: $value:expr;)+) => {
        $($(#[$attr])* const $name: i32 = $value;)+

        /// `code` as a wire `status_t`; `fold` is [`FOLD_UNNAMED_ERRNO`] outside tests.
        #[inline]
        fn status_to_wire(fold: bool, code: StatusCode) -> i32 {
            match code {
                $($(#[$attr])* StatusCode::$variant => $name,)+
                StatusCode::Errno(errno) => errno_to_wire(fold, errno),
                StatusCode::ServiceSpecific(v) => service_specific_to_wire(v),
            }
        }

        /// A wire `status_t` as a `StatusCode`; const patterns let the match lower to a jump table.
        #[inline]
        fn status_from_wire(fold: bool, status: i32) -> StatusCode {
            match status {
                $($(#[$attr])* $name => StatusCode::$variant,)+
                x if x < 0 => errno_from_wire(fold, x),
                x => StatusCode::ServiceSpecific(x),
            }
        }

        #[cfg(test)]
        fn named_wire_codes() -> Vec<(StatusCode, i32)> {
            Vec::from([$($(#[$attr])* (StatusCode::$variant, $name),)+])
        }
    };
}

// AOSP `utils/Errors.h` written out in Linux errno numbering (bionic `asm-generic/errno*.h`).
wire_codes! {
    Ok = OK: 0;
    Unknown = UNKNOWN_ERROR: i32::MIN;
    NoMemory = NO_MEMORY: -12; // -ENOMEM
    InvalidOperation = INVALID_OPERATION: -38; // -ENOSYS
    BadValue = BAD_VALUE: -22; // -EINVAL
    BadType = BAD_TYPE: UNKNOWN_ERROR + 1;
    NameNotFound = NAME_NOT_FOUND: -2; // -ENOENT
    PermissionDenied = PERMISSION_DENIED: -1; // -EPERM
    NoInit = NO_INIT: -19; // -ENODEV
    AlreadyExists = ALREADY_EXISTS: -17; // -EEXIST
    DeadObject = DEAD_OBJECT: -32; // -EPIPE
    FailedTransaction = FAILED_TRANSACTION: UNKNOWN_ERROR + 2;
    UnknownTransaction = UNKNOWN_TRANSACTION: -74; // -EBADMSG
    BadIndex = BAD_INDEX: -75; // -EOVERFLOW
    FdsNotAllowed = FDS_NOT_ALLOWED: UNKNOWN_ERROR + 7;
    UnexpectedNull = UNEXPECTED_NULL: UNKNOWN_ERROR + 8;
    // `+ 9` is AOSP `FROZEN_OBJECT`; `+ 10` is unused there, so no misdecode.
    #[cfg(feature = "rpc")]
    RpcError = RPC_ERROR: UNKNOWN_ERROR + 10;
    NotEnoughData = NOT_ENOUGH_DATA: -61; // -ENODATA
    WouldBlock = WOULD_BLOCK: -11; // -EAGAIN
    TimedOut = TIMED_OUT: -110; // -ETIMEDOUT
    BadFd = BAD_FD: -9; // -EBADF; not in Errors.h, an rsbinder extension
}

/// `Errno(errno)` as a wire status: a payload `>= 0`, or any payload under `fold`, is Unknown.
#[inline]
fn errno_to_wire(fold: bool, errno: i32) -> i32 {
    if errno >= 0 || fold {
        UNKNOWN_ERROR
    } else {
        errno
    }
}

/// An unnamed negative wire status: `Errno` as is, or Unknown under `fold`.
#[inline]
fn errno_from_wire(fold: bool, status: i32) -> StatusCode {
    if fold {
        StatusCode::Unknown
    } else {
        StatusCode::Errno(status)
    }
}

/// `ServiceSpecific(v)` as a wire status: only a positive `v` decodes back to `ServiceSpecific`.
#[inline]
fn service_specific_to_wire(v: i32) -> i32 {
    if v > 0 {
        v
    } else {
        UNKNOWN_ERROR
    }
}

/// Encodes the `status_t` that goes on the wire: a kernel or RPC reply
/// status, or a `StatusCode` written into a [`Parcel`](crate::Parcel).
///
/// A named variant is its AOSP `utils/Errors.h` value in Linux errno
/// numbering on every host. [`StatusCode::BadFd`] (`-EBADF`, `-9`) is an
/// rsbinder extension that `Errors.h` does not define: an AOSP C++ peer
/// receives `-9`, an AOSP NDK or Rust peer reads it as `UNKNOWN_ERROR`.
/// [`StatusCode::Errno`] and [`StatusCode::ServiceSpecific`] follow the
/// rules stated on those variants, so neither encodes as `0`.
/// [`StatusCode::Ok`] is `0`, which the peer reads as success: a handler
/// that returns `Err(StatusCode::Ok)` answers with success.
impl From<StatusCode> for i32 {
    fn from(code: StatusCode) -> Self {
        status_to_wire(FOLD_UNNAMED_ERRNO, code)
    }
}

/// Decodes a wire `status_t`, the inverse of `i32::from(StatusCode)`.
///
/// The argument is a status in AOSP's numbering, not a host errno. The two
/// differ on a host whose errno numbering is not Linux asm-generic (see
/// [`StatusCode::Errno`]): on Apple platforms
/// `StatusCode::from(-libc::ECONNREFUSED)` is [`StatusCode::NotEnoughData`],
/// since `-61` is AOSP `NOT_ENOUGH_DATA`, and an unnamed negative status is
/// [`StatusCode::Unknown`]. Build a status from
/// an OS error with `From<rustix::io::Errno>` or `From<std::io::Error>`,
/// which map the host's errno by name.
impl From<i32> for StatusCode {
    fn from(code: i32) -> Self {
        status_from_wire(FOLD_UNNAMED_ERRNO, code)
    }
}

impl From<std::array::TryFromSliceError> for StatusCode {
    fn from(_: std::array::TryFromSliceError) -> Self {
        StatusCode::NotEnoughData
    }
}

impl From<std::io::Error> for StatusCode {
    fn from(err: std::io::Error) -> Self {
        // Keep the errno (BADF still yields BadFd); no errno, or one rustix rejects, is Unknown.
        match rustix::io::Errno::from_io_error(&err) {
            Some(errno) => StatusCode::from(errno),
            None => StatusCode::Unknown,
        }
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
            _ => unnamed_errno(errno.raw_os_error()),
        }
    }
}

/// A libc-backed `Errno` can hold `0` or a negative value; only a positive errno negates.
fn unnamed_errno(raw: i32) -> StatusCode {
    if raw > 0 {
        StatusCode::Errno(-raw)
    } else {
        StatusCode::Unknown
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
        assert_eq!(code, StatusCode::from(INVALID_OPERATION));
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
        assert_eq!(code, StatusCode::from(UNKNOWN_TRANSACTION));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::UnknownTransaction))
        );

        let code = StatusCode::BadIndex;
        assert_eq!(code, StatusCode::from(BAD_INDEX));
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
        assert_eq!(code, StatusCode::from(NOT_ENOUGH_DATA));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::NotEnoughData))
        );

        let code = StatusCode::WouldBlock;
        assert_eq!(code, StatusCode::from(WOULD_BLOCK));
        assert_eq!(
            code,
            StatusCode::from(Into::<i32>::into(StatusCode::WouldBlock))
        );

        let code = StatusCode::TimedOut;
        assert_eq!(code, StatusCode::from(TIMED_OUT));
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

        // An unnamed errno crosses only on a Linux-numbering host; see `Errno` rustdoc.
        let code: StatusCode = StatusCode::Errno(-5);
        let expected = if FOLD_UNNAMED_ERRNO {
            StatusCode::Unknown
        } else {
            code
        };
        assert_eq!(expected, StatusCode::from(-5));
        assert_eq!(
            expected,
            StatusCode::from(Into::<i32>::into(StatusCode::Errno(-5)))
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

    // Both numberings run on every host: `fold` is an argument, not a `cfg`.
    #[test]
    fn from_i32_round_trip_pins_every_named_variant() {
        for (v, _) in named_wire_codes() {
            for fold in [false, true] {
                let wire = status_to_wire(fold, v);
                assert_eq!(
                    status_from_wire(fold, wire),
                    v,
                    "{v:?} (wire={wire}, {fold})"
                );
            }
            let wire: i32 = v.into();
            assert_eq!(StatusCode::from(wire), v, "{v:?} (wire={wire})");
        }
    }

    /// AOSP `utils/Errors.h` in bionic `asm-generic/errno*.h` numbering, whatever the host.
    #[test]
    fn named_codes_carry_android_wire_values_on_every_host() {
        let pins = [
            (StatusCode::Ok, 0),
            (StatusCode::Unknown, i32::MIN),
            (StatusCode::NoMemory, -12),
            (StatusCode::InvalidOperation, -38),
            (StatusCode::BadValue, -22),
            (StatusCode::BadType, i32::MIN + 1),
            (StatusCode::NameNotFound, -2),
            (StatusCode::PermissionDenied, -1),
            (StatusCode::NoInit, -19),
            (StatusCode::AlreadyExists, -17),
            (StatusCode::DeadObject, -32),
            (StatusCode::FailedTransaction, i32::MIN + 2),
            (StatusCode::UnknownTransaction, -74),
            (StatusCode::BadIndex, -75),
            (StatusCode::FdsNotAllowed, i32::MIN + 7),
            (StatusCode::UnexpectedNull, i32::MIN + 8),
            #[cfg(feature = "rpc")]
            (StatusCode::RpcError, i32::MIN + 10),
            (StatusCode::NotEnoughData, -61),
            (StatusCode::WouldBlock, -11),
            (StatusCode::TimedOut, -110),
            (StatusCode::BadFd, -9),
        ];
        assert_eq!(
            pins.len(),
            named_wire_codes().len(),
            "a named code has no pin"
        );
        for (code, wire) in pins {
            for fold in [false, true] {
                assert_eq!(status_to_wire(fold, code), wire, "{code:?} {fold}");
                assert_eq!(status_from_wire(fold, wire), code, "{wire} {fold}");
            }
            assert_eq!(i32::from(code), wire, "{code:?}");
            assert_eq!(StatusCode::from(wire), code, "{wire}");
        }
    }

    /// Unnamed negative statuses to probe: every errno-sized value plus non-errno AOSP values.
    fn unnamed_probe() -> impl Iterator<Item = i32> {
        let named: Vec<i32> = named_wire_codes().into_iter().map(|(_, w)| w).collect();
        // `UNKNOWN_ERROR + 3..=6` are Win32 Errors.h codes, `+ 9` is `FROZEN_OBJECT`.
        let foreign = [
            -999,
            i32::MIN + 3,
            i32::MIN + 6,
            i32::MIN + 9,
            i32::MIN + 11,
        ];
        (-4096..=-1)
            .chain(foreign)
            .filter(move |x| !named.contains(x))
    }

    /// A Darwin-numbered errno can mean another code in Linux numbering (61), so none goes out.
    #[test]
    fn a_foreign_numbering_host_sends_and_reads_no_unnamed_status() {
        for e in 1..=4096 {
            assert_eq!(
                status_to_wire(true, StatusCode::Errno(-e)),
                UNKNOWN_ERROR,
                "-{e}"
            );
        }
        for x in unnamed_probe() {
            assert_eq!(status_from_wire(true, x), StatusCode::Unknown, "{x}");
        }
    }

    /// Linux numbering is the wire's, so every negative status crosses, as in AOSP C++ libbinder.
    #[test]
    fn a_linux_numbering_host_passes_every_unnamed_status() {
        for e in 1..=4096 {
            assert_eq!(status_to_wire(false, StatusCode::Errno(-e)), -e, "-{e}");
        }
        for x in unnamed_probe() {
            assert_eq!(status_to_wire(false, StatusCode::Errno(x)), x, "{x}");
            assert_eq!(status_from_wire(false, x), StatusCode::Errno(x), "{x}");
        }
    }

    /// Pins which numbering this host uses, independently of `FOLD_UNNAMED_ERRNO`.
    #[test]
    fn the_host_picks_its_numbering() {
        let linux_numbering = cfg!(all(
            any(target_os = "linux", target_os = "android"),
            not(any(
                target_arch = "mips",
                target_arch = "mips64",
                target_arch = "mips32r6",
                target_arch = "mips64r6",
                target_arch = "sparc",
                target_arch = "sparc64",
            ))
        ));
        let refused = StatusCode::from(rustix::io::Errno::CONNREFUSED);
        let host = -rustix::io::Errno::CONNREFUSED.raw_os_error();
        assert_eq!(refused, StatusCode::Errno(host));
        let (wire, back) = if linux_numbering {
            (host, refused)
        } else {
            (UNKNOWN_ERROR, StatusCode::Unknown)
        };
        assert_eq!(i32::from(refused), wire);
        assert_eq!(StatusCode::from(wire), back);
        assert_eq!(
            StatusCode::from(-999),
            if linux_numbering {
                StatusCode::Errno(-999)
            } else {
                StatusCode::Unknown
            }
        );
    }

    /// The fold follows the errno values, so a Linux arch with its own numbering folds too.
    #[test]
    fn the_numbering_rule_compares_errno_values() {
        assert!(numbers_errno_as_asm_generic(&[
            (1, 1),
            (11, 11),
            (111, 111)
        ]));
        // Darwin `EAGAIN` 35, sparc64 `ECONNREFUSED` 61.
        assert!(!numbers_errno_as_asm_generic(&[(1, 1), (35, 11)]));
        assert!(!numbers_errno_as_asm_generic(&[(61, 111), (12, 12)]));
    }

    /// Every pair of the table matches on x86_64 Linux, so the wire carries unnamed errnos.
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    #[test]
    fn x86_64_linux_numbers_errno_as_asm_generic() {
        for (host, generic) in ERRNO_NUMBERING {
            assert_eq!(host, generic);
        }
        // No fold: an unnamed errno (`ECONNRESET`) crosses as is.
        assert_eq!(i32::from(StatusCode::Errno(-104)), -104);
        assert_eq!(StatusCode::from(-104), StatusCode::Errno(-104));
    }

    // Compile-time, so any Apple build of the tests checks it, run or not.
    #[cfg(target_vendor = "apple")]
    const _: () = assert!(
        FOLD_UNNAMED_ERRNO,
        "Darwin errno numbering is not asm-generic"
    );

    /// errno `0` or a value outside an OS's errno range is no errno, and must not panic.
    #[test]
    fn an_io_error_without_a_usable_errno_is_unknown() {
        for raw in [0, -5, i32::MIN] {
            let err = std::io::Error::from_raw_os_error(raw);
            assert_eq!(StatusCode::from(err), StatusCode::Unknown, "{raw}");
        }
        // Above Linux's range: Unknown on linux_raw, a negative `Errno` on a libc backend.
        for raw in [4096, i32::MAX] {
            let code = StatusCode::from(std::io::Error::from_raw_os_error(raw));
            let ok = matches!(code, StatusCode::Unknown | StatusCode::Errno(i32::MIN..=-1));
            assert!(ok, "{raw} -> {code:?}");
        }
        assert_eq!(unnamed_errno(5), StatusCode::Errno(-5));
        for raw in [0, -5, i32::MIN] {
            assert_eq!(unnamed_errno(raw), StatusCode::Unknown, "{raw}");
        }
    }

    #[test]
    fn from_i32_unrecognized_positive_routes_to_service_specific() {
        assert_eq!(StatusCode::from(12345), StatusCode::ServiceSpecific(12345));
        assert_eq!(StatusCode::from(1), StatusCode::ServiceSpecific(1));
        assert_eq!(
            StatusCode::from(i32::MAX),
            StatusCode::ServiceSpecific(i32::MAX)
        );
    }

    /// `0` reads as success and the other sign as another variant, so neither may go out.
    #[test]
    fn a_payload_whose_sign_contradicts_its_variant_goes_out_as_unknown() {
        for fold in [false, true] {
            for v in [0, 1, 32, i32::MAX] {
                assert_eq!(
                    status_to_wire(fold, StatusCode::Errno(v)),
                    UNKNOWN_ERROR,
                    "{v}"
                );
            }
            for v in [0, -1, -9, -74, i32::MIN + 9, i32::MIN] {
                let code = StatusCode::ServiceSpecific(v);
                assert_eq!(status_to_wire(fold, code), UNKNOWN_ERROR, "{v}");
            }
            for v in [1, 32, i32::MAX] {
                let code = StatusCode::ServiceSpecific(v);
                assert_eq!(status_from_wire(fold, status_to_wire(fold, code)), code);
            }
            for v in -4096..=4096 {
                assert_ne!(status_to_wire(fold, StatusCode::Errno(v)), 0, "Errno({v})");
                assert_ne!(status_to_wire(fold, StatusCode::ServiceSpecific(v)), 0);
            }
        }
    }

    /// A status a handler builds from a payload never answers as success, kernel or RPC.
    #[test]
    fn a_status_built_from_a_payload_never_encodes_as_success() {
        use crate::{ExceptionCode, Parcel, Status};
        let codes = [
            StatusCode::Errno(0),
            StatusCode::Errno(32),
            StatusCode::ServiceSpecific(0),
            StatusCode::ServiceSpecific(-74),
            // AOSP `fromExceptionCode(EX_SERVICE_SPECIFIC)`: code 0.
            StatusCode::from(ExceptionCode::ServiceSpecific),
            StatusCode::from(Status::from(ExceptionCode::ServiceSpecific)),
        ];
        for code in codes {
            // A handler's `Err(code)`: the reply status is the code itself.
            assert_ne!(i32::from(code), 0, "{code:?}");
            assert_ne!(
                StatusCode::from(i32::from(code)),
                StatusCode::Ok,
                "{code:?}"
            );
            // A `Status` reply: either the transaction status or a non-zero exception header.
            let mut parcel = Parcel::new();
            match parcel.write(&Status::from(code)) {
                Err(status) => assert_ne!(i32::from(status), 0, "{code:?}"),
                Ok(()) => {
                    parcel.set_data_position(0);
                    assert_ne!(parcel.read::<i32>().unwrap(), 0, "{code:?}");
                }
            }
        }
    }

    /// Host constants and errno range guards in the wire conversion pass on Linux, fail on Apple.
    #[test]
    fn wire_conversion_reads_no_host_constant() {
        let src = include_str!("error.rs");
        let lib = &src[..src.find("\n#[cfg(test)]\nmod tests").expect("test module")];
        // The fold rule is the numbering comparison, never a cfg on vendor, OS or arch.
        let lib_code: String = lib
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        for token in ["target_", "cfg!("] {
            assert!(!lib_code.contains(token), "`{token}` outside the tests");
        }
        assert!(lib.contains(
            "const FOLD_UNNAMED_ERRNO: bool = !numbers_errno_as_asm_generic(&ERRNO_NUMBERING);"
        ));
        let start = lib.find("macro_rules! wire_codes").expect("named table");
        let end = lib
            .find("impl From<std::array::TryFromSliceError>")
            .expect("end of the wire conversion");
        let code: String = lib[start..end]
            .lines()
            .map(|line| line.split("//").next().unwrap_or(""))
            .collect::<Vec<_>>()
            .join("\n");
        for token in [
            "raw_os_error",
            "rustix",
            "libc",
            "Errno::",
            "ELAST",
            "target_",
        ] {
            assert!(!code.contains(token), "`{token}` in the wire conversion");
        }
        // No comparison or range against a non-zero number: `< 0` and `>= 0` are the sign rule.
        let bytes = code.as_bytes();
        for (i, window) in bytes.windows(2).enumerate() {
            let is_bound = matches!(window, [b'<' | b'>', _] | [b'.', b'.']);
            if !is_bound || code[..i].ends_with('-') {
                continue;
            }
            let rest = code[i + 1..].trim_start_matches(['=', '.', ' ', '-']);
            assert!(
                !rest.starts_with(|c: char| c.is_ascii_digit() && c != '0'),
                "range guard at `{}`",
                code[i..].chars().take(12).collect::<String>()
            );
        }
    }

    #[test]
    fn status_code_from_io_error_preserves_errno() {
        let enoent = std::io::Error::from_raw_os_error(rustix::io::Errno::NOENT.raw_os_error());
        assert_eq!(
            StatusCode::from(enoent),
            StatusCode::NameNotFound,
            "ENOENT must map to NameNotFound, not be flattened to BadFd"
        );

        // BADF still resolves to BadFd — via the errno mapping, not a blanket default.
        let ebadf = std::io::Error::from_raw_os_error(rustix::io::Errno::BADF.raw_os_error());
        assert_eq!(StatusCode::from(ebadf), StatusCode::BadFd);

        // A non-OS error has no errno to map → Unknown.
        let non_os = std::io::Error::other("no errno");
        assert_eq!(non_os.raw_os_error(), None);
        assert_eq!(StatusCode::from(non_os), StatusCode::Unknown);
    }
}

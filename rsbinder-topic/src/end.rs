// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The payload of the end record: the [`Status`] the topic ended with.
//!
//! ```text
//! i32 LE   exception code
//! i32 LE   the service-specific code for EX_SERVICE_SPECIFIC, the status code for
//!          EX_TRANSACTION_FAILED, 0 otherwise
//! the message in UTF-8, at most MESSAGE_MAX bytes; absent when the status has none
//! ```
//!
//! These are the three fields of AOSP's `Status`. A topic that ends normally writes exception 0
//! and nothing after the two words.

use rsbinder::{ExceptionCode, Status, StatusCode};

/// The longest message an end record carries; a longer one is cut at a character boundary.
pub(crate) const MESSAGE_MAX: usize = 1024;

const FIELDS: usize = 8;

/// The end record's payload for `status`.
pub(crate) fn payload(status: &Status) -> Vec<u8> {
    let exception = status.exception_code();
    let code = match exception {
        ExceptionCode::ServiceSpecific => status.service_specific_error(),
        ExceptionCode::TransactionFailed => i32::from(status.transaction_error()),
        _ => 0,
    };
    let message = status.message().map(|message| {
        let mut end = message.len().min(MESSAGE_MAX);
        while !message.is_char_boundary(end) {
            end -= 1;
        }
        &message[..end]
    });
    let mut payload = Vec::with_capacity(FIELDS + message.map_or(0, str::len));
    payload.extend_from_slice(&(exception as i32).to_le_bytes());
    payload.extend_from_slice(&code.to_le_bytes());
    payload.extend_from_slice(message.unwrap_or_default().as_bytes());
    payload
}

/// The status an end record's payload holds, or `None` when it is too short to hold one.
///
/// An exception code this version does not know reads as `ExceptionCode::JustError`, and a
/// message that is not UTF-8 is read lossily: the topic has ended either way. An
/// `EX_TRANSACTION_FAILED` status carries no message, as one made from a status code has none.
pub(crate) fn status(payload: &[u8]) -> Option<Status> {
    let (fields, message) = payload.split_at_checked(FIELDS)?;
    let (exception, code) = fields.split_at(4);
    let exception = rsbinder::from_bytes::<ExceptionCode>(exception).ok()?;
    let code = i32::from_le_bytes(code.try_into().ok()?);
    let message = (!message.is_empty()).then(|| String::from_utf8_lossy(message).into_owned());
    Some(match exception {
        ExceptionCode::ServiceSpecific => Status::new_service_specific_error(code, message),
        ExceptionCode::TransactionFailed => Status::from(StatusCode::from(code)),
        exception => match message {
            Some(message) => Status::from((exception, message.as_str())),
            None => Status::from(exception),
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(written: &Status) -> Status {
        status(&payload(written)).expect("a payload `payload` wrote")
    }

    #[test]
    fn each_kind_of_status_reads_back() {
        let ok = round_trip(&Status::from(ExceptionCode::None));
        assert!(ok.is_ok());
        assert_eq!(ok.message(), None);

        let specific = round_trip(&Status::new_service_specific_error(7, Some("full".into())));
        assert_eq!(specific.exception_code(), ExceptionCode::ServiceSpecific);
        assert_eq!(specific.service_specific_error(), 7);
        assert_eq!(specific.message(), Some("full"));

        let failed = round_trip(&Status::from(StatusCode::DeadObject));
        assert_eq!(failed.exception_code(), ExceptionCode::TransactionFailed);
        assert_eq!(failed.transaction_error(), StatusCode::DeadObject);

        let state = round_trip(&Status::from((ExceptionCode::IllegalState, "stopped")));
        assert_eq!(state.exception_code(), ExceptionCode::IllegalState);
        assert_eq!(state.message(), Some("stopped"));
    }

    #[test]
    fn a_long_message_is_cut_at_a_character_boundary() {
        // Three bytes a character: 1024 is not a boundary, 1023 is.
        let message = "가".repeat(400);
        let read = round_trip(&Status::from((ExceptionCode::Security, message.as_str())));
        assert_eq!(read.message(), Some(&message[..1023]));
    }

    #[test]
    fn a_payload_a_publisher_forged_still_ends_the_topic() {
        assert!(status(&[0; 7]).is_none());
        let mut forged = (-1000i32).to_le_bytes().to_vec();
        forged.extend_from_slice(&0i32.to_le_bytes());
        forged.extend_from_slice(&[0xff, b'x']);
        let read = status(&forged).expect("eight bytes and more");
        assert_eq!(read.exception_code(), ExceptionCode::JustError);
        assert_eq!(read.message(), Some("\u{fffd}x"));
    }
}

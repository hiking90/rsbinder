// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a topic carries: [`Payload`], its implementation for every AIDL parcelable, and [`Bytes`].

use rsbinder::{Deserialize, ParcelableMetadata, Result, Serialize};

/// A value a topic carries: a name both ends compare, and the codec that turns it into the bytes
/// of one record and back.
///
/// Every AIDL parcelable implements it — a type generated from `.aidl` or derived with
/// `#[derive(rsbinder::Parcelable)]` — with the parcelable's descriptor as the name and
/// [`rsbinder::to_bytes`] and [`rsbinder::from_bytes`] as the codec. A parcelable writes a length
/// header, so a subscriber built against an older definition reads a record that has fields
/// appended after it, as it would over binder. [`Bytes`] carries bytes as they are.
///
/// A type of the application's own may implement it to use another codec. The name is all the
/// two ends compare when a subscription is set up, so two implementations that share a name and
/// differ in codec connect, and every record then fails to decode on the subscriber.
///
/// There is no implementation for `i32`, `String` and the like. A topic of one such value wraps
/// it in a parcelable, or carries it as [`Bytes`].
pub trait Payload: Sized {
    /// The name the publisher and the subscriber compare when a subscription is set up.
    fn type_name() -> &'static str;

    /// Append the record's bytes to `out`. What `out` holds already belongs to other records and
    /// must stay as it is.
    ///
    /// # Errors
    ///
    /// Any error makes the publish fail with it and write nothing.
    fn encode(&self, out: &mut Vec<u8>) -> Result<()>;

    /// Read a value from the bytes of one record, all of them.
    ///
    /// # Errors
    ///
    /// Any error is returned to the subscriber for that record alone; the next one is read as
    /// usual.
    fn decode(bytes: &[u8]) -> Result<Self>;
}

/// An AIDL parcelable. A binder or a file descriptor in the value cannot be carried, since
/// neither means anything in the subscriber's process: [`rsbinder::to_bytes`] refuses the first
/// with `BadType` and the second with `FdsNotAllowed`.
impl<T: Serialize + Deserialize + ParcelableMetadata> Payload for T {
    fn type_name() -> &'static str {
        T::descriptor()
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&rsbinder::to_bytes(self)?);
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        rsbinder::from_bytes(bytes)
    }
}

/// Bytes carried as they are, under the type name `"bytes"`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Payload for Bytes {
    fn type_name() -> &'static str {
        "bytes"
    }

    fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        out.extend_from_slice(&self.0);
        Ok(())
    }

    fn decode(bytes: &[u8]) -> Result<Self> {
        Ok(Self(bytes.to_vec()))
    }
}

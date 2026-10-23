// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What a topic carries: [`Payload`], its implementation for every AIDL parcelable, and [`Bytes`];
//! and what a codec writes to and reads from, [`Encoding`] and [`Record`].

use rsbinder::{Deserialize, ParcelableMetadata, Result, Serialize};

/// A value a topic carries: a name both ends compare, and the codec that turns it into the bytes
/// of one record and back.
///
/// Every AIDL parcelable implements it, with the parcelable's descriptor as the name and
/// [`rsbinder::to_bytes`]' encoding as the codec. A topic between Rust processes defines its type
/// with `#[derive(rsbinder::Parcelable)]` (rsbinder's `macros` feature) and no `.aidl`: the derive
/// emits the code `.aidl` generates, so the record is the same either way. The descriptor names
/// the package, since the derive's default is the bare type name, which a `Sample` of another
/// crate shares:
///
/// ```
/// #[derive(rsbinder::Parcelable, Debug, Default)]
/// #[parcelable(descriptor = "my.package.Reading")]
/// struct Reading {
///     seq: i64,
///     values: Vec<f64>,
/// }
///
/// fn type_name<T: rsbinder_topic::Payload>() -> &'static str {
///     T::type_name()
/// }
/// assert_eq!(type_name::<Reading>(), "my.package.Reading");
/// ```
///
/// A `.aidl` parcelable is for a type whose definition another language shares, so that one file
/// generates both ends; a shape the derive cannot express, a union for one, is written there too
/// or implements this trait itself. A parcelable writes a length header, so a subscriber built
/// against an older definition reads a record that has fields appended after it, as it would over
/// binder. [`Bytes`] carries bytes as they are.
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

    /// Write the record's bytes to `out`, in order, with its methods.
    ///
    /// # Errors
    ///
    /// Any error makes the publish fail with it and write nothing.
    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> Result<()>;

    /// Read a value from one record, all of its bytes.
    ///
    /// # Errors
    ///
    /// Any error is returned to the subscriber for that record alone; the next one is read as
    /// usual.
    fn decode(record: &mut Record<'_>) -> Result<Self>;
}

/// An AIDL parcelable. A binder or a file descriptor in the value cannot be carried, since
/// neither means anything in the subscriber's process: [`rsbinder::to_bytes`] refuses the first
/// with `BadType` and the second with `FdsNotAllowed`.
///
/// The type name is the parcelable's descriptor, which for a generic parcelable (`Foo<T>`) names
/// no type argument, as in AOSP's generated code, where `ParcelableHolder` carries and compares
/// that string. So `Foo<A>` and `Foo<B>` have one type name, and a subscriber of one is taken by
/// a publisher of the other, whose records then fail to decode or decode wrong. A topic of a
/// generic parcelable that must not meet another instance wraps it in a type of its own that
/// implements `Payload` with a name that says the arguments:
///
/// ```ignore
/// struct Pairs(Pair<i64>);
///
/// impl rsbinder_topic::Payload for Pairs {
///     fn type_name() -> &'static str {
///         "my.package.Pair<long>"
///     }
///     fn encode<'a>(&'a self, out: &mut rsbinder_topic::Encoding<'a>) -> rsbinder::Result<()> {
///         out.parcelable(&self.0)
///     }
///     fn decode(record: &mut rsbinder_topic::Record<'_>) -> rsbinder::Result<Self> {
///         record.parcelable().map(Self)
///     }
/// }
/// ```
impl<T: Serialize + Deserialize + ParcelableMetadata> Payload for T {
    fn type_name() -> &'static str {
        T::descriptor()
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> Result<()> {
        out.parcelable(self)
    }

    fn decode(record: &mut Record<'_>) -> Result<Self> {
        record.parcelable()
    }
}

/// Bytes carried as they are, under the type name `"bytes"`. A publish copies them once, into
/// the ring; a receive copies them once, out of it, and the value takes that copy.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Bytes(pub Vec<u8>);

impl Payload for Bytes {
    fn type_name() -> &'static str {
        "bytes"
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> Result<()> {
        out.borrow(&self.0);
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> Result<Self> {
        Ok(Self(record.take()))
    }
}

/// Where [`Payload::encode`] writes a record: its bytes are what the calls give, in order.
///
/// Nothing is in the ring until every value of the publish is encoded; then the publisher, under
/// its lock, copies each record into the ring. [`borrow`](Self::borrow) leaves bytes where they
/// are until that copy, which is the only one they get. [`extend`](Self::extend) and
/// [`parcelable`](Self::parcelable) write to a buffer the publisher keeps from one publish to the
/// next, so they cost a copy more and, once the buffer has grown, no allocation.
pub struct Encoding<'a> {
    pub(crate) bytes: Vec<u8>,
    pub(crate) pieces: Vec<Piece<'a>>,
    /// Where the record being encoded starts in `pieces`.
    first: usize,
    /// Bytes of the record being encoded.
    pub(crate) len: usize,
}

/// A run of a record's bytes: in [`Encoding::bytes`], or borrowed from the value.
#[derive(Clone, Copy, Debug)]
pub(crate) enum Piece<'a> {
    Kept { start: usize, end: usize },
    Borrowed(&'a [u8]),
}

impl<'a> Encoding<'a> {
    /// Append `bytes`, which the publisher copies into the ring with no copy before.
    pub fn borrow(&mut self, bytes: &'a [u8]) {
        if !bytes.is_empty() {
            self.len += bytes.len();
            self.pieces.push(Piece::Borrowed(bytes));
        }
    }

    /// Append a copy of `bytes`.
    pub fn extend(&mut self, bytes: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(bytes);
        self.kept(start);
    }

    /// Append `value` as [`rsbinder::to_bytes`] encodes it.
    ///
    /// # Errors
    ///
    /// Those of [`rsbinder::to_bytes`]; nothing is appended then.
    pub fn parcelable<T: Serialize + ?Sized>(&mut self, value: &T) -> Result<()> {
        let start = self.bytes.len();
        rsbinder::to_bytes_into(value, &mut self.bytes)?;
        self.kept(start);
        Ok(())
    }

    /// Count what `bytes` holds past `start` as the record's next bytes.
    fn kept(&mut self, start: usize) {
        let end = self.bytes.len();
        if end == start {
            return;
        }
        self.len += end - start;
        // Two appends in a row of one record are one run: the last run then ends at `start`.
        if self.pieces.len() > self.first {
            if let Some(Piece::Kept { end: last, .. }) = self.pieces.last_mut() {
                if *last == start {
                    *last = end;
                    return;
                }
            }
        }
        self.pieces.push(Piece::Kept { start, end });
    }

    /// An encoding into the allocations of `bytes` and `pieces`, which it empties.
    pub(crate) fn new(mut bytes: Vec<u8>, pieces: Vec<Piece<'_>>) -> Self {
        bytes.clear();
        Self {
            bytes,
            pieces: relabel(pieces),
            first: 0,
            len: 0,
        }
    }

    /// Start the next record: what follows is its bytes.
    pub(crate) fn begin(&mut self) {
        self.first = self.pieces.len();
        self.len = 0;
    }

    /// The bytes of `piece`.
    pub(crate) fn of(&self, piece: Piece<'a>) -> &[u8] {
        match piece {
            Piece::Kept { start, end } => &self.bytes[start..end],
            Piece::Borrowed(bytes) => bytes,
        }
    }
}

/// The record [`Payload::decode`] reads: its bytes, copied out of the ring into a buffer the
/// subscriber keeps from one record to the next.
pub struct Record<'r> {
    buffer: &'r mut Vec<u8>,
    len: usize,
}

impl std::fmt::Debug for Record<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Record").field("len", &self.len).finish()
    }
}

impl<'r> Record<'r> {
    /// The record of the first `len` bytes of `buffer`.
    pub(crate) fn new(buffer: &'r mut Vec<u8>, len: usize) -> Self {
        debug_assert!(len <= buffer.len());
        Self { buffer, len }
    }

    /// The record's bytes.
    pub fn bytes(&self) -> &[u8] {
        &self.buffer[..self.len]
    }

    /// The record's bytes, as a buffer the caller now owns: no copy. The subscriber allocates
    /// another for its next record, and [`bytes`](Self::bytes) is empty from here on.
    pub fn take(&mut self) -> Vec<u8> {
        self.buffer.truncate(self.len);
        self.len = 0;
        std::mem::take(self.buffer)
    }

    /// The value [`rsbinder::from_bytes`] decodes from the record's bytes, read where they are.
    ///
    /// # Errors
    ///
    /// Those of [`rsbinder::from_bytes`].
    pub fn parcelable<T: Deserialize>(&mut self) -> Result<T> {
        // The buffer holds the record padded to 8; the decoder reads all it is given.
        self.buffer.truncate(self.len);
        rsbinder::from_bytes_in(self.buffer)
    }
}

/// `pieces`, emptied, for another lifetime: the allocation goes with it.
#[expect(
    clippy::unnecessary_filter_map,
    reason = "the item type changes lifetime, which `filter` cannot do"
)]
pub(crate) fn relabel<'b>(mut pieces: Vec<Piece<'_>>) -> Vec<Piece<'b>> {
    pieces.clear();
    // An empty vector collected in place: std keeps the allocation (same size and alignment).
    pieces.into_iter().filter_map(|_| None).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relabel_keeps_the_allocation() {
        let mut pieces = Vec::with_capacity(16);
        pieces.push(Piece::Borrowed(&b"x"[..]));
        let at = pieces.as_ptr() as usize;
        let relabelled: Vec<Piece<'static>> = relabel(pieces);
        assert_eq!((relabelled.len(), relabelled.capacity()), (0, 16));
        assert_eq!(relabelled.as_ptr() as usize, at);
    }
}

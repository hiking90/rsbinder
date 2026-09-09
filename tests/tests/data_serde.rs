// Copyright 2025 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `rsbinder::to_bytes` / `from_bytes` over real parcelables.
//!
//! The unit tests beside the implementation cover the primitives and the
//! refusals. What needs a parcelable to show is the property the API is
//! actually sold on: that a stored value can be read back by code built
//! against a *different* version of its definition. That comes from the
//! length header a parcelable writes, not from anything in `to_bytes`.

#![cfg(all(feature = "rpc", feature = "macros"))]

use rsbinder::{from_bytes, to_bytes, BinderEnum, Parcelable};

#[derive(Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "dataserde.Inner")]
pub struct Inner {
    pub id: i32,
    pub label: String,
}

#[derive(BinderEnum, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Mode {
    #[default]
    Off = 0,
    On = 1,
}

/// Version 1 of a stored record.
#[derive(Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "dataserde.Record")]
pub struct RecordV1 {
    pub flag: bool,
    pub small: i8,
    pub code: i32,
    pub big: i64,
    pub ratio: f64,
    pub name: String,
    pub blob: Vec<u8>,
    pub codes: Vec<i32>,
    pub names: Vec<String>,
    pub maybe: Option<String>,
    pub mode: Mode,
    pub inner: Inner,
    pub inners: Vec<Inner>,
}

/// The same record after someone appended fields — the only shape of
/// change a positional format can absorb.
#[derive(Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "dataserde.Record")]
pub struct RecordV2 {
    pub flag: bool,
    pub small: i8,
    pub code: i32,
    pub big: i64,
    pub ratio: f64,
    pub name: String,
    pub blob: Vec<u8>,
    pub codes: Vec<i32>,
    pub names: Vec<String>,
    pub maybe: Option<String>,
    pub mode: Mode,
    pub inner: Inner,
    pub inners: Vec<Inner>,
    pub added_count: i32,
    pub added_note: String,
}

fn sample_v1() -> RecordV1 {
    RecordV1 {
        flag: true,
        small: -2,
        code: 0x0102_0304,
        big: i64::MIN + 7,
        ratio: -0.5,
        name: "한 quiet".into(),
        blob: vec![0xAB, 0xCD, 0xEF],
        codes: vec![1, -2, 3],
        names: vec!["a".into(), "".into()],
        maybe: Some("present".into()),
        mode: Mode::On,
        inner: Inner {
            id: 9,
            label: "in".into(),
        },
        inners: vec![
            Inner {
                id: 1,
                label: "x".into(),
            },
            Inner::default(),
        ],
    }
}

#[test]
fn every_field_shape_survives_the_round_trip() {
    let value = sample_v1();
    let bytes = to_bytes(&value).expect("encode");
    assert_eq!(from_bytes::<RecordV1>(&bytes).expect("decode"), value);
    // `code` is on these bytes as 04 03 02 01. This suite runs on
    // little-endian hosts only, so it rules out a byte-swapping encoder,
    // not a host-native one; the cross-endian proof is `parcel`'s absolute
    // goldens, which the s390x job runs.
    assert!(
        bytes.windows(4).any(|w| w == [0x04, 0x03, 0x02, 0x01]),
        "0x01020304 must be encoded little-endian"
    );

    // A `None` option is a field too, and its absence has to round-trip
    // as such rather than as an empty string.
    let mut absent = value;
    absent.maybe = None;
    assert_eq!(
        from_bytes::<RecordV1>(&to_bytes(&absent).expect("encode")).expect("decode"),
        absent
    );
}

#[test]
fn a_reader_built_against_the_older_definition_still_reads_the_new_bytes() {
    let base = sample_v1();
    let v2 = RecordV2 {
        flag: base.flag,
        small: base.small,
        code: base.code,
        big: base.big,
        ratio: base.ratio,
        name: base.name.clone(),
        blob: base.blob.clone(),
        codes: base.codes.clone(),
        names: base.names.clone(),
        maybe: base.maybe.clone(),
        mode: base.mode,
        inner: base.inner.clone(),
        inners: base.inners.clone(),
        added_count: 42,
        added_note: "appended".into(),
    };
    let v1: RecordV1 = from_bytes(&to_bytes(&v2).expect("encode")).expect("decode");

    // Every field the old definition knows about is intact — not one of
    // them is a default, so a decode that returned defaults would fail
    // here. The two it does not know are simply not read: the
    // parcelable's length header stops it at the boundary the writer
    // wrote.
    assert_eq!(v1, base);
}

#[test]
fn a_reader_built_against_the_newer_definition_defaults_what_is_missing() {
    let v1 = sample_v1();
    let v2: RecordV2 = from_bytes(&to_bytes(&v1).expect("encode")).expect("decode");

    assert_eq!(v2.code, v1.code);
    assert_eq!(v2.inners, v1.inners);
    assert_eq!(v2.added_count, 0, "a field the writer never wrote");
    assert_eq!(v2.added_note, "");
}

#[test]
fn a_record_written_as_one_type_is_not_silently_read_as_another() {
    // The bytes of a `RecordV1` are longer than an `i32`, and stopping
    // after the first four would be a plausible-looking wrong answer.
    let bytes = to_bytes(&sample_v1()).expect("encode");
    assert!(
        from_bytes::<i32>(&bytes).is_err(),
        "trailing bytes must not be ignored"
    );
}

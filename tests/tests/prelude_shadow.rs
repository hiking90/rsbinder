// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AIDL names that are also std prelude items (`Ok`, `Err`, `Some`, `None`, `Default`,
//! `Option`, `Vec`, `Box`, `String`) become items of the generated module and shadow the
//! prelude there. The generated code must name the prelude items by path, or this file
//! does not compile.

#![allow(non_snake_case)]

use rsbinder::{Parcel, Parcelable};

include!(concat!(env!("OUT_DIR"), "/prelude_shadow.rs"));

use preludeshadow::IShadow;
use preludeshadow::ShadowParcelable::{self, ShadowParcelable as Shadow};
use preludeshadow::ShadowTypes::ShadowTypes;
use preludeshadow::ShadowUnion::{self, ShadowUnion as Union};

fn round_trip<T>(value: &T) -> T
where
    T: Parcelable + Default,
{
    let mut parcel = Parcel::new();
    value.write_to_parcel(&mut parcel).unwrap();
    parcel.set_data_position(0);
    let mut back = T::default();
    back.read_from_parcel(&mut parcel).unwrap();
    back
}

#[test]
fn shadowing_constants_keep_their_values() {
    let parcelable = [
        ShadowParcelable::Ok,
        ShadowParcelable::Err,
        ShadowParcelable::Some,
        ShadowParcelable::None,
    ];
    let union = [
        ShadowUnion::Ok,
        ShadowUnion::Err,
        ShadowUnion::Some,
        ShadowUnion::None,
    ];
    let interface = [IShadow::Ok, IShadow::Err, IShadow::Some, IShadow::None];
    assert_eq!(parcelable, [1, 2, 3, 4]);
    assert_eq!(union, [1, 2, 3, 4]);
    assert_eq!(interface, [0, 1, 2, 3]);
}

#[test]
fn parcelable_with_a_nested_default_type_round_trips() {
    let value = Shadow::default();
    assert_eq!(value.values, Some(vec![1, 2]));
    assert_eq!(value.names, Some(vec![Some("a".to_owned())]));

    let mut value = value;
    value.text = "text".into();
    value.nested.x = 7;
    let back = round_trip(&value);
    assert_eq!(back.text, "text");
    assert_eq!(back.nested.x, 7);
    assert_eq!(back.values, Some(vec![1, 2]));
}

/// Nested `Option`/`Vec`/`Box`/`String` types beside fields of the std types.
#[test]
fn parcelable_with_nested_std_named_types_round_trips() {
    let mut value = ShadowTypes {
        values: vec![1, 2, 3],
        text: "text".into(),
        maybe: Some("maybe".into()),
        ..Default::default()
    };
    value.option.x = 5;
    value.next = Some(Box::new(ShadowTypes {
        values: vec![4],
        ..Default::default()
    }));
    let back = round_trip(&value);
    assert_eq!(back.values, [1, 2, 3]);
    assert_eq!(
        (back.text.as_str(), back.maybe.as_deref()),
        ("text", Some("maybe"))
    );
    assert_eq!(back.option.x, 5);
    assert_eq!(back.next.expect("next").values, [4]);
}

#[test]
fn union_with_shadowing_constants_round_trips() {
    assert!(matches!(Union::default(), Union::Values(Some(ref v)) if v == &[1, 2]));
    let back = round_trip(&Union::Text("text".into()));
    assert!(matches!(back, Union::Text(ref s) if s == "text"));
}

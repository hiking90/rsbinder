// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What `#[derive(BinderEnum)]` puts on the wire, checked by running it.
//!
//! The unit tests next to the macro can only read the tokens it emits; these
//! evaluate them. Nothing here touches a transport — the enum codec is the
//! same whichever one carries it.

use rsbinder::BinderEnum;

#[derive(BinderEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i32)]
pub enum Mode {
    Fast = 0,
    Safe = 1,
}

/// A derived enum is closed: a value no variant declares is rejected rather
/// than carried along, which is the one place it parts company with `.aidl`.
#[test]
fn rejects_an_undeclared_value() {
    assert_eq!(Mode::try_from_binder_value(0), Ok(Mode::Fast));
    assert_eq!(Mode::try_from_binder_value(1), Ok(Mode::Safe));
    assert_eq!(
        Mode::try_from_binder_value(7),
        Err(rsbinder::StatusCode::BadValue)
    );
    assert_eq!(Mode::Safe.binder_value(), 1);
}

#[derive(BinderEnum, Clone, Copy, PartialEq, Eq, Debug)]
#[repr(i64)]
pub enum Wide {
    Bit31 = 1 << 31,
    Bit40 = 1 << 40,
    Negative = -5,
}

/// The wire value is the one the declaration shows. Re-emitting the
/// discriminant expression instead would type it on its own, where an integer
/// literal falls back to `i32`: `1 << 31` would go out as `-2147483648`, and
/// `1 << 40` would not even compile.
#[test]
fn a_wide_repr_keeps_the_declared_value() {
    for (declared, value) in [
        (Wide::Bit31 as i64, Wide::Bit31.binder_value()),
        (Wide::Bit40 as i64, Wide::Bit40.binder_value()),
        (Wide::Negative as i64, Wide::Negative.binder_value()),
    ] {
        assert_eq!(declared, value);
    }
    assert_eq!(Wide::Bit31.binder_value(), 2_147_483_648);
    assert_eq!(Wide::try_from_binder_value(1 << 40), Ok(Wide::Bit40));
}

/// A binder enum asks nothing of the user's type beyond its `repr`: the codec
/// matches on the variant rather than casting through a shared reference.
#[derive(BinderEnum, Clone, PartialEq, Eq, Debug)]
#[repr(i8)]
pub enum NotCopy {
    One = 1,
}

#[test]
fn does_not_require_copy_at_runtime() {
    assert_eq!(NotCopy::One.binder_value(), 1);
    assert_eq!(NotCopy::try_from_binder_value(1), Ok(NotCopy::One));
}

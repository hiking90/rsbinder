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

/// A derived enum is closed, unlike `.aidl`'s: an undeclared value is rejected.
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

/// Re-emitted as `i32`, `1 << 31` would go out negative and `1 << 40` would not compile.
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

/// Not `Copy`: the codec matches on the variant rather than casting through `&self`.
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

/// Items named like the prelude's; the derive output must not resolve to them.
#[allow(dead_code)]
mod shadowed {
    pub struct Option;
    pub struct Vec;
    pub struct Ok;
    pub struct Err;

    #[derive(rsbinder::BinderEnum, PartialEq, Eq, Debug)]
    #[repr(i32)]
    pub enum Level {
        Low = 0,
    }
}

#[test]
fn a_module_shadowing_the_prelude_still_derives() {
    use shadowed::Level;
    assert_eq!(Level::try_from_binder_value(0), Ok(Level::Low));
    assert_eq!(
        Level::try_from_binder_value(9),
        Err(rsbinder::StatusCode::BadValue)
    );
}

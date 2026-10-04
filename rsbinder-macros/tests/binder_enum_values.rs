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

    // The codec's `Ok(())` must reach `core`, not the struct above.
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Point {
        pub x: i32,
    }
}

/// The `#[interface]` module glob-imports its parent; these names must not reach its body.
#[allow(dead_code, unused_imports)]
mod shadowed_interface {
    pub struct Ok;
    pub struct Err;
    pub struct Option;
    pub struct String;
    // No `Box`: `async-trait`'s expansion names it bare.
    pub trait Send {}
    pub trait Sync {}
    pub mod std {}
    pub enum Mode {
        None,
        Some,
        Default,
    }
    pub use Mode::*;

    #[rsbinder::interface]
    pub trait IShadowed {
        fn get(&self) -> rsbinder::BinderResult<i32>;
        fn put(&self, v: i32) -> rsbinder::BinderResult<()>;
        // An `out` argument starts from `Default::default()`; a variable one sends `Some(len)`.
        fn fill(&self, v: &mut Vec<i32>) -> rsbinder::BinderResult<()>;
        fn wide(&self, v: &mut [i32; 33]) -> rsbinder::BinderResult<()>;
    }
}

/// User types named like prelude items, in signatures: the generated module must resolve them.
#[allow(dead_code)]
mod prelude_named_types {
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Ok {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Err {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Some {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct None {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Default {
        pub v: i32,
    }

    #[rsbinder::interface]
    pub trait IPreludeNamed {
        fn ok(&self) -> rsbinder::BinderResult<Ok>;
        fn err(&self, v: &Err) -> rsbinder::BinderResult<Some>;
        fn none(&self, v: &mut None) -> rsbinder::BinderResult<()>;
        fn dflt(&self, #[inout] v: &mut Default) -> rsbinder::BinderResult<Vec<Default>>;
    }
}

/// User types named like the async half's helper struct and type parameters, in signatures.
#[allow(dead_code)]
mod async_helper_named_types {
    #[derive(rsbinder::Parcelable, Default)]
    pub struct Wrapper {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct T {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct R {
        pub v: i32,
    }
    #[derive(rsbinder::Parcelable, Default)]
    pub struct P {
        pub v: i32,
    }

    #[rsbinder::interface]
    pub trait IHelperNamed {
        fn wrap(&self, w: &Wrapper) -> rsbinder::BinderResult<Wrapper>;
        fn t(&self, v: &T) -> rsbinder::BinderResult<R>;
        fn p(&self, #[inout] v: &mut P) -> rsbinder::BinderResult<Vec<P>>;
    }
}

/// The async view's method takes the user's `P`, not the pool type parameter.
#[test]
fn a_user_type_named_like_an_async_helper_reaches_the_async_signature() {
    use async_helper_named_types::{IHelperNamedAsync, P};
    fn returns_user_p<'a, X: IHelperNamedAsync<rsbinder::Tokio> + ?Sized>(
        x: &'a X,
        v: &'a mut P,
    ) -> rsbinder::BoxFuture<'a, rsbinder::BinderResult<Vec<P>>> {
        x.p(v)
    }
    let _ = returns_user_p::<dyn IHelperNamedAsync<rsbinder::Tokio>>;
}

/// The generated proxy hands back the user's `Ok`, not `core::result::Result::Ok`.
#[test]
fn a_user_type_named_like_a_prelude_item_reaches_the_signature() {
    fn returns_user_ok<T: prelude_named_types::IPreludeNamed + ?Sized>(
        t: &T,
    ) -> rsbinder::BinderResult<prelude_named_types::Ok> {
        t.ok()
    }
    let _ = returns_user_ok::<dyn prelude_named_types::IPreludeNamed>;
}

/// The fixed-size array shapes AOSP renders compile through the generated proxy and stub.
mod fixed_array_shapes {
    #[rsbinder::interface]
    pub trait IFixed {
        fn a(
            &self,
            #[inout] v: &mut [rsbinder::ParcelFileDescriptor; 3],
        ) -> rsbinder::BinderResult<()>;
        fn b(&self, #[inout] v: &mut [[rsbinder::SIBinder; 2]; 2]) -> rsbinder::BinderResult<()>;
        fn c(&self, v: Option<&[Option<String>; 3]>) -> rsbinder::BinderResult<()>;
        fn d(&self) -> rsbinder::BinderResult<Option<[Option<rsbinder::ParcelFileDescriptor>; 2]>>;
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

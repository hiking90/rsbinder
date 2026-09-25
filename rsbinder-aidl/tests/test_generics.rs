// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Generic parcelables (`parcelable Foo<@FixedSize T, U>`), their use sites
//! (`Foo<byte, Bar>`), and the builtin `android.hardware.common` types the
//! runtime crate provides (`MQDescriptor`, `NativeHandle`).

use rsbinder_aidl::{parse_document, AidlError, Builder, Generator, SourceContext};
use similar::{ChangeTag, TextDiff};
use std::path::PathBuf;

fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn generate(input: &str) -> Result<String, AidlError> {
    let ctx = SourceContext::new("test.aidl", input);
    let document = parse_document(&ctx)?;
    Generator::new(false, false)
        .document(&document)
        .map(|res| res.1)
}

fn assert_generated(input: &str, expect: &str) {
    let res = generate(input).unwrap();
    let diff = TextDiff::from_lines(res.trim(), expect.trim());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
            ChangeTag::Equal => "  ",
        };
        print!("{sign}{change}");
    }
    assert_eq!(res.trim(), expect.trim());
}

fn error_message(input: &str) -> String {
    match generate(input) {
        Ok(out) => panic!("expected an error, generated:\n{out}"),
        Err(e) => format!("{e:?}"),
    }
}

/// A generic parcelable: every parameter becomes a `PhantomData` field and
/// every impl is generic over it, as AOSP's Rust backend emits it. The
/// parameters have no bound: they never reach the parcel.
#[test]
fn generic_parcelable_emits_phantom_fields() {
    assert_generated(
        r#"
        package android.aidl.tests;
        @JavaDerive(toString=true)
        parcelable GenericStructuredParcelable<T, U, B> {
            int a;
            int b;
        }
        "#,
        r#"
pub mod GenericStructuredParcelable {
    #![allow(clippy::all, unused_imports, non_upper_case_globals, non_snake_case, dead_code, deprecated)]
    #[derive(Debug)]
    pub struct GenericStructuredParcelable<T, U, B> {
        pub r#a: i32,
        pub r#b: i32,
        pub _phantom_T: core::marker::PhantomData<T>,
        pub _phantom_U: core::marker::PhantomData<U>,
        pub _phantom_B: core::marker::PhantomData<B>,
    }
    impl<T, U, B> Default for GenericStructuredParcelable<T, U, B> {
        fn default() -> Self {
            Self {
                r#a: Default::default(),
                r#b: Default::default(),
                _phantom_T: core::marker::PhantomData,
                _phantom_U: core::marker::PhantomData,
                _phantom_B: core::marker::PhantomData,
            }
        }
    }
    impl<T, U, B> rsbinder::Parcelable for GenericStructuredParcelable<T, U, B> {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_write(|_sub_parcel| {
                _sub_parcel.write(&self.r#a)?;
                _sub_parcel.write(&self.r#b)?;
                Ok(())
            })
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.sized_read(|_sub_parcel| {
                if !_sub_parcel.has_more_data() { return Ok(()); }
                self.r#a = _sub_parcel.read()?;
                if !_sub_parcel.has_more_data() { return Ok(()); }
                self.r#b = _sub_parcel.read()?;
                Ok(())
            })
        }
    }
    rsbinder::impl_serialize_for_parcelable!(GenericStructuredParcelable<T, U, B>);
    rsbinder::impl_deserialize_for_parcelable!(GenericStructuredParcelable<T, U, B>);
    impl<T, U, B> rsbinder::ParcelableMetadata for GenericStructuredParcelable<T, U, B> {
        fn descriptor() -> &'static str { "android.aidl.tests.GenericStructuredParcelable" }
    }
}
        "#,
    );
}

/// Type arguments reach the Rust path: bare, array/`List` element, nested, under field `@nullable`.
#[test]
fn type_arguments_reach_the_rust_path() {
    let out = generate(
        r#"
        package p;
        parcelable Box<T> { int a; }
        parcelable Pair<A, B> { int a; }
        enum Kind { A, B }
        parcelable Holder {
            Box<int> one;
            Box<Kind>[] many;
            List<Box<String>> listed;
            Box<Box<int>> nested;
            Pair<int, Box<int>> mixed;
            @nullable Box<int> maybe;
        }
        interface IUser {
            Box<int> get(in Pair<int, Kind> p, out Box<int>[] out_boxes);
        }
        "#,
    )
    .unwrap();
    for expected in [
        "pub r#one: super::Box::Box<i32>,",
        "pub r#many: Vec<super::Box::Box<super::Kind::Kind>>,",
        "pub r#listed: Vec<super::Box::Box<String>>,",
        "pub r#nested: super::Box::Box<super::Box::Box<i32>>,",
        "pub r#mixed: super::Pair::Pair<i32, super::Box::Box<i32>>,",
        "pub r#maybe: Option<super::Box::Box<i32>>,",
        "_arg_p: &super::Pair::Pair<i32, super::Kind::Kind>",
        "_arg_out_boxes: &mut Vec<super::Box::Box<i32>>",
        "rsbinder::BinderResult<super::Box::Box<i32>>",
    ] {
        assert!(out.contains(expected), "missing `{expected}` in:\n{out}");
    }
}

/// `@FixedSize T` is a requirement on the argument: a `byte` and a
/// `@FixedSize` parcelable satisfy it, a `String` does not.
#[test]
fn fixed_size_requirement_is_checked_at_the_use_site() {
    assert!(generate(
        r#"
        package p;
        parcelable Q<@FixedSize T, F> { int a; }
        @FixedSize parcelable Elem { int x; }
        enum Flavor { EMPTY }
        parcelable Use { Q<byte, Flavor> a; Q<Elem, Flavor> b; }
        "#,
    )
    .is_ok());

    let msg = error_message(
        r#"
        package p;
        parcelable Q<@FixedSize T, F> { int a; }
        enum Flavor { EMPTY }
        parcelable Use { Q<String, Flavor> a; }
        "#,
    );
    assert!(
        msg.contains(
            "type 'String' used as type parameter 'T' of 'Q' must be annotated with @FixedSize"
        ),
        "{msg}"
    );

    // A generic instantiation is never fixed size, whatever its declaration
    // is annotated (AOSP `CanBeFixedSize`: `IsGeneric()` first).
    let msg = error_message(
        r#"
        package p;
        parcelable Q<@FixedSize T, F> { int a; }
        @FixedSize parcelable Elem<T> { int x; }
        enum Flavor { EMPTY }
        parcelable Use { Q<Elem<int>, Flavor> a; }
        "#,
    );
    assert!(
        msg.contains("used as type parameter 'T' of 'Q' must be annotated with @FixedSize"),
        "{msg}"
    );
    let msg = error_message(
        r#"
        package p;
        @FixedSize parcelable Elem<T> { int x; }
        @FixedSize parcelable Outer { Elem<int> f; }
        "#,
    );
    assert!(msg.contains("FixedSizeNonFixedField"), "{msg}");
}

/// Every type argument's annotation is refused at parse time; AOSP refuses only the first's.
#[test]
fn type_argument_cannot_be_annotated() {
    for input in [
        "package p; parcelable Q<T, F> { int a; } parcelable E { int x; } enum F { A } \
         parcelable Use { Q<@nullable E, F> a; }",
        "package p; parcelable Q<T, F> { int a; } parcelable E { int x; } \
         parcelable Use { Q<E, @nullable E> a; }",
        "package p; parcelable Use { Map<String, @nullable String> a; }",
        "package p; parcelable Use { List<@nullable String> a; }",
        "package p; parcelable Q<T> { int a; } parcelable Use { Q<@utf8InCpp String> a; }",
    ] {
        let ctx = SourceContext::new("test.aidl", input);
        let msg = format!("{:?}", parse_document(&ctx).unwrap_err());
        assert!(
            msg.contains("Annotations for type arguments are not supported"),
            "{msg}"
        );
    }
}

/// `List` takes one argument; the grammar admits more and AOSP refuses them
/// (`aidl_language.cpp`: "List can only have one type parameter").
#[test]
fn list_takes_exactly_one_type_argument() {
    let msg = error_message(
        r#"
        package p;
        parcelable Use { List<String, int> a; }
        "#,
    );
    assert!(
        msg.contains("List can only have one type parameter, but got 2"),
        "{msg}"
    );
}

#[test]
fn vintf_stability_requirement_is_checked_at_the_use_site() {
    assert!(generate(
        r#"
        package p;
        parcelable Q<@VintfStability T> { int a; }
        @VintfStability parcelable Stable { int x; }
        parcelable Use { Q<Stable> a; }
        "#,
    )
    .is_ok());
    let msg = error_message(
        r#"
        package p;
        parcelable Q<@VintfStability T> { int a; }
        parcelable Unstable { int x; }
        parcelable Use { Q<Unstable> a; }
        "#,
    );
    assert!(
        msg.contains("type 'Unstable' used as type parameter 'T' of 'Q' must be annotated with @VintfStability"),
        "{msg}"
    );
}

/// A generic parcelable held by value is an edge of the sizing graph like
/// any parcelable: a cycle closed only through `Foo<int>` fields is boxed
/// when `@nullable` and rejected when not.
#[test]
fn a_cycle_through_generic_parcelables_is_boxed_or_rejected() {
    let out = generate(
        r#"
        package p;
        parcelable A<T> { @nullable B<int> b; }
        parcelable B<T> { @nullable A<int> a; }
        "#,
    )
    .unwrap();
    assert!(
        out.contains("pub r#b: Option<Box<super::B::B<i32>>>,"),
        "{out}"
    );
    assert!(
        out.contains("pub r#a: Option<Box<super::A::A<i32>>>,"),
        "{out}"
    );

    let msg = error_message(
        r#"
        package p;
        parcelable A<T> { B<int> b; }
        parcelable B<T> { A<int> a; }
        "#,
    );
    assert!(
        msg.contains("RecursiveParcelable { type_name: \"B\""),
        "{msg}"
    );

    // A `List` element stays behind its allocation and closes nothing.
    assert!(generate(
        r#"
        package p;
        parcelable A<T> { List<B<int>> b; }
        parcelable B<T> { List<A<int>> a; }
        "#,
    )
    .is_ok());
}

#[test]
fn argument_count_must_match_the_declaration() {
    let msg = error_message(
        r#"
        package p;
        parcelable Q<T, F> { int a; }
        parcelable Use { Q<int> a; }
        "#,
    );
    assert!(
        msg.contains("'Q' must have 2 type parameters, but got 1"),
        "{msg}"
    );

    let msg = error_message(
        r#"
        package p;
        parcelable Q<T, F> { int a; }
        parcelable Use { Q a; }
        "#,
    );
    assert!(
        msg.contains("'Q' must have 2 type parameters, but got 0"),
        "{msg}"
    );

    let msg = error_message(
        r#"
        package p;
        parcelable Plain { int a; }
        parcelable Use { Plain<int> a; }
        "#,
    );
    assert!(msg.contains("'Plain' is not a generic type"), "{msg}");
}

#[test]
fn type_argument_cannot_be_an_array_list_or_void() {
    for (arg, what) in [
        ("int[]", "an array or List"),
        ("List<String>", "an array or List"),
    ] {
        let msg = error_message(&format!(
            r#"
            package p;
            parcelable Q<T> {{ int a; }}
            parcelable Use {{ Q<{arg}> a; }}
            "#
        ));
        assert!(
            msg.contains(&format!("a type argument cannot be {what}")),
            "{msg}"
        );
    }
}

/// The parameter list itself: only `@FixedSize` and `@VintfStability` may
/// annotate a parameter, and a name may not repeat.
#[test]
fn type_parameter_declaration_is_validated() {
    let ctx = SourceContext::new(
        "test.aidl",
        "package p; parcelable Q<@nullable T> { int a; }",
    );
    let msg = format!("{:?}", parse_document(&ctx).unwrap_err());
    assert!(
        msg.contains("'@nullable' cannot annotate the type parameter 'T'"),
        "{msg}"
    );

    let ctx = SourceContext::new("test.aidl", "package p; parcelable Q<T, T> { int a; }");
    let msg = format!("{:?}", parse_document(&ctx).unwrap_err());
    assert!(msg.contains("type parameter 'T' is repeated"), "{msg}");

    // The Java-only annotations AOSP admits here are accepted and ignored:
    // they state no requirement on the argument.
    assert!(generate(
        r#"
        package p;
        parcelable Q<@JavaSuppressLint(value={"NewApi"}) T, @JavaPassthrough(annotation="@X") U> { int a; }
        parcelable Use { Q<String, IBinder> a; }
        "#,
    )
    .is_ok());

    // A parameter is spelled bare in the generated Rust; the generator refuses these names.
    for (name, what) in [
        ("String", "a name the generated Rust spells unqualified"),
        ("Vec", "a name the generated Rust spells unqualified"),
        ("i32", "a name the generated Rust spells unqualified"),
        ("type", "a Rust keyword"),
        ("rsbinder", "the runtime crate's path"),
        ("Q", "the declaration it belongs to"),
    ] {
        let msg = error_message(&format!("package p; parcelable Q<T, {name}> {{ int a; }}"));
        assert!(
            msg.contains(&format!("type parameter '{name}' of 'Q' shadows {what}")),
            "{msg}"
        );
    }

    // AOSP: "Generic types can't have nested types." A nested declaration is
    // reachable from a field only by a bare-headed path (`Bar::Baz::Baz`),
    // which a parameter named `Bar` would capture.
    let msg = error_message(
        r#"
        package p;
        parcelable Foo<Bar> {
            Bar.Baz z;
            parcelable Bar { parcelable Baz { int x; } }
        }
        "#,
    );
    assert!(
        msg.contains("generic types can't have nested types: 'Foo' declares 'Bar'"),
        "{msg}"
    );
}

/// `ParcelableHolder` is a nameable type, so it can be a type argument (AOSP
/// accepts it; only a field, array or `List` of it is restricted).
#[test]
fn parcelable_holder_can_be_a_type_argument() {
    let out = generate(
        r#"
        package p;
        parcelable Q<T> { int a; }
        parcelable Use { Q<ParcelableHolder> a; }
        "#,
    )
    .unwrap();
    assert!(
        out.contains("pub r#a: super::Q::Q<rsbinder::ParcelableHolder>,"),
        "{out}"
    );
}

/// A field cannot have a parameter's type: nothing would be written for it.
#[test]
fn type_parameter_cannot_be_a_field_type() {
    let msg = error_message(
        r#"
        package p;
        parcelable Q<T> { T value; }
        "#,
    );
    assert!(
        msg.contains("field 'value' of 'Q' has the type parameter 'T' as its type"),
        "{msg}"
    );
    let msg = error_message(
        r#"
        package p;
        parcelable Q<T> { T[] values; }
        "#,
    );
    assert!(
        msg.contains("field 'values' of 'Q' has the type parameter 'T'"),
        "{msg}"
    );
}

/// `pub type Foo<T> = X;` cannot be emitted without knowing `X`'s shape.
#[test]
fn generic_rust_type_parcelable_is_rejected() {
    let msg = error_message(
        r#"
        package p;
        parcelable Q<T> rust_type "crate::Q";
        "#,
    );
    assert!(
        msg.contains("parcelable 'Q' is generic and names a `rust_type`"),
        "{msg}"
    );
    // `std` is spelled bare by the fixed-array default (`std::array::from_fn`).
    let msg = error_message(
        r#"
        package p;
        parcelable Q<std> { int[4] a; }
        "#,
    );
    assert!(
        msg.contains(
            "type parameter 'std' of 'Q' shadows a name the generated Rust spells unqualified"
        ),
        "{msg}"
    );
}

#[test]
fn generic_union_is_rejected() {
    let msg = error_message(
        r#"
        package p;
        union U<T> { int a; long b; }
        "#,
    );
    assert!(msg.contains("union 'U' is generic"), "{msg}");
}

/// An imported builtin FMQ type maps to the runtime crate, enforces `@FixedSize`, emits no module.
#[test]
fn builtin_fmq_types_map_to_the_runtime_crate() {
    let dir = scratch_dir("builtin_fmq");
    let src = dir.join("fmqdemo").join("IFmqDemo.aidl");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(
        &src,
        r#"
        package fmqdemo;
        import android.hardware.common.fmq.MQDescriptor;
        import android.hardware.common.fmq.SynchronizedReadWrite;
        import android.hardware.common.fmq.UnsynchronizedWrite;
        import android.hardware.common.NativeHandle;

        @VintfStability
        parcelable Bundle {
            MQDescriptor<int, SynchronizedReadWrite> queue;
            NativeHandle handle;
            MQDescriptor<byte, UnsynchronizedWrite>[] more;
        }

        interface IFmqDemo {
            MQDescriptor<byte, SynchronizedReadWrite> getQueue();
            void setQueue(in MQDescriptor<byte, SynchronizedReadWrite> desc);
            Bundle getBundle();
        }
        "#,
    )
    .unwrap();
    let out = dir.join("out.rs");
    Builder::new()
        .source(&src)
        .output(&out)
        .dest_dir(&dir)
        .generate()
        .unwrap();
    let generated = std::fs::read_to_string(&out).unwrap();
    for expected in [
        "pub r#queue: rsbinder::fmq::MQDescriptor<i32, rsbinder::fmq::SynchronizedReadWrite>,",
        "pub r#handle: rsbinder::NativeHandle,",
        "pub r#more: Vec<rsbinder::fmq::MQDescriptor<i8, rsbinder::fmq::UnsynchronizedWrite>>,",
        "rsbinder::BinderResult<rsbinder::fmq::MQDescriptor<i8, rsbinder::fmq::SynchronizedReadWrite>>",
        "_arg_desc: &rsbinder::fmq::MQDescriptor<i8, rsbinder::fmq::SynchronizedReadWrite>",
    ] {
        assert!(generated.contains(expected), "missing `{expected}` in:\n{generated}");
    }
    assert!(
        !generated.contains("pub mod MQDescriptor") && !generated.contains("pub mod android"),
        "the builtin must not be generated:\n{generated}"
    );

    // `@FixedSize T` on the builtin's declaration is enforced.
    std::fs::write(
        &src,
        r#"
        package fmqdemo;
        import android.hardware.common.fmq.MQDescriptor;
        import android.hardware.common.fmq.SynchronizedReadWrite;
        interface IFmqDemo {
            MQDescriptor<String, SynchronizedReadWrite> getQueue();
        }
        "#,
    )
    .unwrap();
    let err = Builder::new()
        .source(&src)
        .output(&out)
        .dest_dir(&dir)
        .generate()
        .unwrap_err();
    let msg = format!("{err:?}");
    assert!(
        msg.contains("type 'String' used as type parameter 'T' of 'MQDescriptor' must be annotated with @FixedSize"),
        "{msg}"
    );
}

/// A vendored copy under an include directory wins over the builtin: it is
/// compiled like any other source and referenced through its own module.
#[test]
fn a_vendored_source_takes_precedence_over_the_builtin() {
    let dir = scratch_dir("builtin_override");
    let vendored = dir
        .join("android")
        .join("hardware")
        .join("common")
        .join("NativeHandle.aidl");
    std::fs::create_dir_all(vendored.parent().unwrap()).unwrap();
    std::fs::write(
        &vendored,
        "package android.hardware.common; @VintfStability parcelable NativeHandle { ParcelFileDescriptor[] fds; int[] ints; }",
    )
    .unwrap();
    let src = dir.join("demo").join("Use.aidl");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    std::fs::write(
        &src,
        "package demo; import android.hardware.common.NativeHandle; parcelable Use { NativeHandle h; }",
    )
    .unwrap();
    let out = dir.join("out.rs");
    Builder::new()
        .source(&src)
        .include_dir(&dir)
        .output(&out)
        .dest_dir(&dir)
        .generate()
        .unwrap();
    let generated = std::fs::read_to_string(&out).unwrap();
    assert!(
        generated.contains(
            "pub r#h: super::super::android::hardware::common::NativeHandle::NativeHandle,"
        ),
        "{generated}"
    );
    assert!(generated.contains("pub mod NativeHandle"), "{generated}");
}

/// A vendored copy that only a later source's include dir reveals still wins over the builtin.
#[test]
fn a_copy_discovered_by_a_later_source_still_takes_precedence() {
    let dir = scratch_dir("builtin_late_include");
    // `src/A.aidl` with `package demo;`: `src` is not `…/demo`, so no
    // include directory is derived from it.
    let a = dir.join("src").join("A.aidl");
    std::fs::create_dir_all(a.parent().unwrap()).unwrap();
    std::fs::write(
        &a,
        "package demo; import android.hardware.common.NativeHandle; parcelable A { NativeHandle h; }",
    )
    .unwrap();
    // `vendor/demo2/B.aidl` derives `vendor/`, which also holds the copy.
    let b = dir.join("vendor").join("demo2").join("B.aidl");
    std::fs::create_dir_all(b.parent().unwrap()).unwrap();
    std::fs::write(&b, "package demo2; parcelable B { int x; }").unwrap();
    let vendored = dir
        .join("vendor")
        .join("android")
        .join("hardware")
        .join("common")
        .join("NativeHandle.aidl");
    std::fs::create_dir_all(vendored.parent().unwrap()).unwrap();
    std::fs::write(
        &vendored,
        "package android.hardware.common; @VintfStability parcelable NativeHandle { ParcelFileDescriptor[] fds; int[] ints; }",
    )
    .unwrap();
    let out = dir.join("out.rs");
    Builder::new()
        .source(&a)
        .source(&b)
        .output(&out)
        .dest_dir(&dir)
        .generate()
        .unwrap();
    let generated = std::fs::read_to_string(&out).unwrap();
    assert!(
        generated.contains(
            "pub r#h: super::super::android::hardware::common::NativeHandle::NativeHandle,"
        ),
        "{generated}"
    );
    assert!(!generated.contains("rsbinder::NativeHandle"), "{generated}");
}

/// The vendored copy also wins when a builtin imports it: `MQDescriptor`
/// stays the runtime crate's type while `NativeHandle` is compiled from the
/// include directory, whichever import is resolved first.
#[test]
fn a_vendored_dependency_of_a_builtin_takes_precedence() {
    let dir = scratch_dir("builtin_dependency_override");
    let vendored = dir
        .join("android")
        .join("hardware")
        .join("common")
        .join("NativeHandle.aidl");
    std::fs::create_dir_all(vendored.parent().unwrap()).unwrap();
    std::fs::write(
        &vendored,
        "package android.hardware.common; @VintfStability parcelable NativeHandle { ParcelFileDescriptor[] fds; int[] ints; }",
    )
    .unwrap();
    let src = dir.join("demo").join("Use.aidl");
    std::fs::create_dir_all(src.parent().unwrap()).unwrap();
    let out = dir.join("out.rs");
    for (source, field) in [
        (
            r#"
            package demo;
            import android.hardware.common.NativeHandle;
            import android.hardware.common.fmq.MQDescriptor;
            import android.hardware.common.fmq.SynchronizedReadWrite;
            parcelable Use { NativeHandle h; MQDescriptor<byte, SynchronizedReadWrite> q; }
            "#,
            Some("pub r#h: super::super::android::hardware::common::NativeHandle::NativeHandle,"),
        ),
        // The builtin's own import is the only mention of `NativeHandle`.
        (
            r#"
            package demo;
            import android.hardware.common.fmq.MQDescriptor;
            import android.hardware.common.fmq.SynchronizedReadWrite;
            parcelable Use { MQDescriptor<byte, SynchronizedReadWrite> q; }
            "#,
            None,
        ),
    ] {
        std::fs::write(&src, source).unwrap();
        Builder::new()
            .source(&src)
            .include_dir(&dir)
            .output(&out)
            .dest_dir(&dir)
            .generate()
            .unwrap();
        let generated = std::fs::read_to_string(&out).unwrap();
        for expected in [
            "pub r#q: rsbinder::fmq::MQDescriptor<i8, rsbinder::fmq::SynchronizedReadWrite>,",
            "pub mod NativeHandle",
        ]
        .into_iter()
        .chain(field)
        {
            assert!(
                generated.contains(expected),
                "missing `{expected}` in:\n{generated}"
            );
        }
        assert!(
            !generated.contains("rsbinder::NativeHandle")
                && !generated.contains("pub mod MQDescriptor"),
            "{generated}"
        );
    }
}

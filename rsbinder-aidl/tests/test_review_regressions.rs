// SPDX-License-Identifier: Apache-2.0

//! Regression tests for codegen defects.
//! Each case is parseable input that must surface as a recoverable error (or
//! compute without panicking) rather than aborting the AIDL compiler — the
//! project's "no panic on user input" invariant.

/// Returns `true` only when BOTH parsing and code generation succeed.
fn generate_ok(input: &str) -> bool {
    generate_str(input).is_some()
}

/// Generated Rust on success; the generator does not type-check, so assert on the text itself.
fn generate_str(input: &str) -> Option<String> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).ok()?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).ok().map(|(_, rust)| rust)
}

/// `List<T[]>` is grammar-valid but must be a diagnostic, never `type_decl()`'s array panic.
#[test]
fn list_of_array_is_rejected_not_panicked() {
    for src in [
        "parcelable P { List<int[]> field; }",
        "interface I { void m(in List<String[]> a); }",
        "interface I2 { List<int[]> r(); }",
    ] {
        assert!(
            !generate_ok(src),
            "expected list-of-array to error: {src:?}"
        );
    }
    assert!(
        generate_ok("parcelable P { List<int> field; }"),
        "legitimate List<int> must still generate"
    );
}

/// The constant cycle guard must see through binary operators, or the recursion overflows.
#[test]
fn cyclic_constants_do_not_overflow() {
    // Reaching the end of this call without aborting is the assertion.
    let _ = generate_ok("interface ICycle { const int A = B + 1; const int B = A + 1; }");

    assert!(
        generate_ok("interface IOk { const int A = 1; const int B = A + 1; const int C = A + B; }"),
        "non-cyclic constant chain must still resolve"
    );
}

/// AOSP `previous + 1` auto-increment: past `i64::MAX` it is an overflow diagnostic, not a wrap.
#[test]
fn enum_autoincrement_overflow_is_rejected() {
    let src = "@Backing(type=\"long\") enum Big { MAXV = 9223372036854775807, NEXT }";
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", src);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let err = rsbinder_aidl::Generator::new(false, false)
        .document(&document)
        .expect_err("auto-increment past i64::MAX must be rejected");
    assert!(format!("{err:?}").contains("overflows"), "got: {err:?}");

    let src = "@Backing(type=\"long\") enum Big { MAXV = 9223372036854775807, NEXT = 0, N2 }";
    let out = generate_str(src).expect("an explicit value after i64::MAX must still generate");
    assert!(out.contains("r#N2 = 1,"), "got: {out}");
}

/// `{}` where no aggregate is valid is a diagnostic, not an `unwrap()` panic; `int[] x = {}` is ok.
#[test]
fn empty_brace_initializer_is_rejected_not_panicked() {
    for src in [
        "enum E { A = {} }",                    // enumerator value
        "parcelable P { int[] x = {{}}; }",     // nested array element
        "@Foo({}) parcelable P { int x; }",     // annotation argument
        "@Foo(bar={}) parcelable P { int x; }", // named annotation parameter
        "parcelable P { int[{}] x; }",          // array dimension
    ] {
        assert!(
            !generate_ok(src),
            "empty `{{}}` initializer must error, not panic: {src:?}"
        );
    }
    assert!(
        generate_ok("parcelable P { int[] x = {}; }"),
        "a legitimate empty-array initializer must still generate"
    );
}

/// A byte array's element is `u8`, so negatives are re-emitted unsigned (AOSP `aidl_to_rust.cpp`).
#[test]
fn negative_byte_array_default_emits_unsigned() {
    let out = generate_str("parcelable P { byte[] a = {-1, -2, 3}; byte[2] f = {-1, 127}; }")
        .expect("must generate");
    let packed = out.replace([' ', '\n'], "");
    assert!(
        packed.contains("vec![255,254,3,]"),
        "Vec<u8> byte default must reinterpret negatives as u8 (got: {packed})"
    );
    assert!(
        packed.contains("[255,127,]"),
        "[u8; N] byte default must reinterpret negatives as u8 (got: {packed})"
    );
    assert!(
        !packed.contains("vec![-1,") && !packed.contains("[-1,"),
        "no negated literal may remain in a u8 byte array (got: {packed})"
    );
}

/// A non-finite float default (`1.0e400`) emits `f64::INFINITY`-style constants, not `inff64`.
#[test]
fn non_finite_float_default_emits_valid_constant() {
    let out =
        generate_str("parcelable P { double d = 1.0e400; float f = 1.0e400; double n = 1.0; }")
            .expect("must generate");
    let packed = out.replace([' ', '\n'], "");
    assert!(
        packed.contains("f64::INFINITY"),
        "double infinity default must emit f64::INFINITY (got: {packed})"
    );
    assert!(
        packed.contains("f32::INFINITY"),
        "float infinity default must emit f32::INFINITY (got: {packed})"
    );
    assert!(
        !packed.contains("inff64") && !packed.contains("inff32"),
        "no `inff64`/`inff32` token may remain (got: {packed})"
    );
    // A finite default is untouched.
    assert!(
        packed.contains("1f64") || packed.contains("1.0f64"),
        "finite double default keeps suffixed-decimal form (got: {packed})"
    );
}

/// `Option` only for lack of `Default`: like AOSP, a null member is UNEXPECTED_NULL both ways.
#[test]
fn union_non_nullable_binder_member_is_null_strict() {
    let out = generate_str(
        "package test.u;\nunion U { IBinder b; ParcelFileDescriptor pfd; int n; @nullable IBinder maybe; }",
    )
    .expect("must generate");
    let packed = out.replace([' ', '\n'], "");
    // Write side: unwrap with UnexpectedNull for each non-nullable member.
    assert!(
        packed.contains(
            "Self::r#B(v)=>{parcel.write(&0i32)?;\
             parcel.write(v.as_ref().ok_or(rsbinder::StatusCode::UnexpectedNull)?)}"
        ),
        "non-nullable IBinder union member write must unwrap with UnexpectedNull (got: {packed})"
    );
    assert!(
        packed.contains(
            "Self::r#Pfd(v)=>{parcel.write(&1i32)?;\
             parcel.write(v.as_ref().ok_or(rsbinder::StatusCode::UnexpectedNull)?)}"
        ),
        "non-nullable PFD union member write must unwrap with UnexpectedNull (got: {packed})"
    );
    // The @nullable member keeps the permissive plain write.
    assert!(
        packed.contains("Self::r#Maybe(v)=>{parcel.write(&3i32)?;parcel.write(v)}"),
        "nullable union member must keep the plain write (got: {packed})"
    );
    // Read side: an inbound null is rejected on the non-nullable arms only.
    assert!(
        packed.contains(
            "ifvalue.is_none(){returnErr(rsbinder::StatusCode::UnexpectedNull);}\
             *self=Self::r#B(value)"
        ),
        "non-nullable IBinder union member read must reject null (got: {packed})"
    );
    assert!(
        packed.contains(
            "ifvalue.is_none(){returnErr(rsbinder::StatusCode::UnexpectedNull);}\
             *self=Self::r#Pfd(value)"
        ),
        "non-nullable PFD union member read must reject null (got: {packed})"
    );
    assert!(
        packed.contains("parcel.read()?;*self=Self::r#Maybe(value)"),
        "nullable union member read must stay permissive (got: {packed})"
    );
}

/// Parcelable read side of the same contract: an inbound null in a non-nullable field is refused.
#[test]
fn parcelable_non_nullable_binder_field_read_is_null_strict() {
    let out = generate_str(
        "package test.p;\nparcelable P { IBinder b; @nullable IBinder maybe; int n; }",
    )
    .expect("must generate");
    let packed = out.replace([' ', '\n'], "");
    assert!(
        packed.contains("ifself.r#b.is_none(){returnErr(rsbinder::StatusCode::UnexpectedNull);}"),
        "non-nullable parcelable field read must reject null (got: {packed})"
    );
    assert!(
        !packed.contains("ifself.r#maybe.is_none()"),
        "nullable field must stay permissive on read (got: {packed})"
    );
    assert!(
        !packed.contains("ifself.r#n.is_none()"),
        "primitive field must not get a null check (got: {packed})"
    );
}

/// A bad array dimension is a diagnostic (AOSP): folding to 0 would demote it to a `Vec<T>` wire.
#[test]
fn bad_fixed_array_dimension_is_diagnostic() {
    // Unresolvable dimension constant.
    assert!(
        !generate_ok("parcelable P { int[NO_SUCH_CONST] a; }"),
        "unresolvable array dimension must error"
    );
    // Evaluation failure inside the dimension expression.
    assert!(
        !generate_ok("parcelable P { int[1/0] a; }"),
        "failing array dimension expression must error"
    );
    // Non-positive dimensions.
    assert!(
        !generate_ok("parcelable P { int[0] a; }"),
        "zero array dimension must error"
    );
    assert!(
        !generate_ok("parcelable P { int[-1] a; }"),
        "negative array dimension must error"
    );
    // Non-integral dimension (`to_i64` would silently truncate 1.9 to 1).
    assert!(
        !generate_ok("parcelable P { int[1.9] a; }"),
        "float array dimension must error"
    );
    // A valid constant dimension still works and stays a fixed array.
    let out = generate_str("parcelable P { const int SIZE = 3; int[SIZE] a; }")
        .expect("valid dimension must generate");
    assert!(
        out.contains("[i32; 3]"),
        "fixed dimension must stay a fixed array (got: {out})"
    );
}

/// Arity mismatches and array-on-scalar defaults are diagnostics, else rustc rejects the output.
#[test]
fn array_literal_shape_mismatches_are_diagnostics() {
    assert!(
        !generate_ok("parcelable P { int[2] a = {1,2,3}; }"),
        "fixed-array arity mismatch must error"
    );
    assert!(
        !generate_ok("parcelable P { int x = {}; }"),
        "empty array literal on a scalar field must error"
    );
    assert!(
        !generate_ok("interface IFoo { const int A = {}; }"),
        "empty array literal on a scalar constant must error"
    );
    // A matching fixed-array default still generates.
    let out = generate_str("parcelable P { int[2] a = {1,2}; }").expect("must generate");
    assert!(out.contains("[1,2,]"), "got: {out}");
}

/// Member names self/Self/super/crate/_ have no raw-identifier form, so parsing rejects them.
#[test]
fn reserved_path_keyword_member_names_are_diagnostics() {
    for src in [
        "interface IFoo { const int self = 1; }",
        "parcelable P { int crate; }",
        "parcelable P { int _; }",
        "enum E { _ }",
        "interface IFoo { void _(); }",
        "parcelable Foo<_> { int a; }",
    ] {
        assert!(!generate_ok(src), "reserved name must error: {src}");
    }
}

/// Unary operators on string literals are diagnostics (AOSP); a pass-through drops the operator.
#[test]
fn unary_operator_on_string_is_diagnostic() {
    for src in [
        "interface IFoo { const String S = -\"x\"; }",
        "interface IFoo { const String S = ~\"x\"; }",
        "interface IFoo { const String S = +\"x\"; }",
    ] {
        assert!(!generate_ok(src), "string unary must error: {src}");
    }
}

/// `const String[]` renders as `&[&str]`: string literals do not coerce to `&[String]` in a const.
#[test]
fn const_string_array_renders_as_str_slice() {
    let out = generate_str("interface IFoo { const String[] S = {\"a\",\"b\"}; }")
        .expect("must generate");
    assert!(
        out.contains(r#"r#S: &[&str] = &["a","b",];"#),
        "const String[] must emit &[&str] (got: {out})"
    );
}

/// A stale pre-registration cache entry would fold `A = X, B` to A=5, B=5 (a duplicate).
#[test]
fn enum_discriminant_referencing_interface_constant_auto_increments() {
    let ctx = rsbinder_aidl::SourceContext::new(
        "t.aidl",
        "package test.e;\ninterface IFoo { const int X = 5; enum E { A = X, B } }",
    );
    let doc = rsbinder_aidl::parse_document(&ctx).expect("must parse");
    // Same two-pass flow as Builder::generate.
    rsbinder_aidl::Generator::pre_register_enums(&doc);
    let gen = rsbinder_aidl::Generator::new(false, false);
    let out = gen.document(&doc).expect("must generate").1;
    assert!(out.contains("r#A = 5,"), "got: {out}");
    assert!(
        out.contains("r#B = 6,"),
        "auto-increment must continue from the reference (got: {out})"
    );
}

/// Float/char discriminants are errors, not `to_i64` truncations; AOSP treats bool as integral.
#[test]
fn non_integral_enum_discriminants_are_diagnostics() {
    assert!(
        !generate_ok("enum F { A = 1.5, B }"),
        "float discriminant must error"
    );
    assert!(
        !generate_ok("enum G { A = 'a' }"),
        "char discriminant must error"
    );
    assert!(
        generate_ok("enum H { A = (~(-1)) == 0, B = 1 == 1 }"),
        "bool-valued comparison discriminants are AOSP-legal"
    );
}

/// AOSP `ClassName` strips a leading `I` only before an uppercase letter (`Foo3` -> `BnFoo3`).
#[test]
fn bn_bp_names_follow_aosp_i_prefix_rule() {
    let out = generate_str("package test.n;\ninterface Foo3 { void m(); }").expect("must generate");
    assert!(out.contains("BnFoo3"), "expected BnFoo3 (got: {out})");
    assert!(out.contains("BpFoo3"), "expected BpFoo3 (got: {out})");
    assert!(
        !out.contains("Bnoo3"),
        "must not strip a non-I prefix (got: {out})"
    );

    let out = generate_str("package test.n;\ninterface IFoo { void m(); }").expect("must generate");
    assert!(
        out.contains("BnFoo"),
        "I-prefixed name keeps stripping (got: {out})"
    );

    // Lowercase after `I` is not the AOSP I-prefix convention.
    let out = generate_str("package test.n;\ninterface Ifoo { void m(); }").expect("must generate");
    assert!(out.contains("BnIfoo"), "expected BnIfoo (got: {out})");
}

/// Constant names stay verbatim (AOSP Rust backend); upper-casing collides `foo`/`FOO` (E0428).
#[test]
fn const_names_are_verbatim_not_uppercased() {
    let out = generate_str(
        "package test.c;\ninterface IFoo { const int kMagicValue = 7; const int foo = 1; const int FOO = 2; }",
    )
    .expect("must generate");
    assert!(
        out.contains("pub const r#kMagicValue: i32 = 7"),
        "constant name must stay verbatim (got: {out})"
    );
    assert!(
        out.contains("pub const r#foo: i32 = 1") && out.contains("pub const r#FOO: i32 = 2"),
        "distinct-case constants must not collide (got: {out})"
    );
    assert!(
        !out.contains("KMAGICVALUE"),
        "no upper-cased rename may remain (got: {out})"
    );
}

/// A default of an unconvertible type is an AIDL diagnostic (as in AOSP), not a rustc error.
#[test]
fn type_mismatched_default_is_diagnostic() {
    assert!(
        !generate_ok("interface IFoo { const int A = \"x\"; }"),
        "string default on an int constant must error"
    );
    assert!(
        !generate_ok("parcelable P { int[] a = {\"x\"}; }"),
        "string element in an int array default must error"
    );
    // Sanity: well-typed defaults still generate.
    assert!(generate_ok("interface IFoo { const int A = 3; }"));
}

/// AOSP has one expression grammar, so `A + "y"` and `("y" + "z")` concat like any operand.
#[test]
fn string_concat_composes_like_aosp() {
    let out =
        generate_str("interface IFoo { const String A = \"x\"; const String B = A + \"y\"; }")
            .expect("reference-first string concat must parse");
    assert!(out.contains(r#"r#B: &str = "xy""#), "got: {out}");

    let out = generate_str("interface IFoo { const String C = (\"y\" + \"z\"); }")
        .expect("parenthesized string concat must parse");
    assert!(out.contains(r#"r#C: &str = "yz""#), "got: {out}");

    // Literal-first concat keeps working.
    let out = generate_str("interface IFoo { const String D = \"a\" + \"b\"; }")
        .expect("literal-first concat must parse");
    assert!(out.contains(r#"r#D: &str = "ab""#), "got: {out}");
}

/// `const T[] X = {};` emits `&[]`: `Default::default()` is not const for `&[T]` (E0658/E0015).
#[test]
fn const_array_emits_slice_literal() {
    let out = generate_str("interface IFoo { const int[] A = {}; const int[] B = {1,2}; }")
        .expect("const arrays must generate");
    let packed = out.replace([' ', '\n'], "");
    assert!(
        packed.contains("r#A:&[i32]=&[]"),
        "empty const array must emit &[] (got: {packed})"
    );
    assert!(
        packed.contains("r#B:&[i32]=&[1,2,]"),
        "const array must emit a slice literal (got: {packed})"
    );
    assert!(
        !packed.contains("r#A:&[i32]=Default::default()") && !packed.contains("=vec!["),
        "no vec!/Default::default() const initializers may remain (got: {packed})"
    );
}

/// A `//` comment may end at EOF: LINE_COMMENT does not require a trailing `\n`.
#[test]
fn trailing_line_comment_without_newline_parses() {
    assert!(
        generate_ok("interface IFoo { void m(); }\n// trailing comment"),
        "EOF-terminated line comment must parse"
    );
    assert!(
        generate_ok("interface IFoo { void m(); } // same-line trailing"),
        "same-line EOF comment must parse"
    );
}

// ---- Codegen/API shape defects ----

/// An unqualified constant resolves in its own or an enclosing scope, never a same-named other.
#[test]
fn unqualified_constant_does_not_leak_across_declarations() {
    let out = generate_str(
        "package test;\n\
         interface IX { const int A = 999; void z(); }\n\
         parcelable P { const int A = 1; const int B = A + 1; }",
    )
    .expect("must generate");
    assert!(out.contains("pub const r#B: i32 = 2;"), "got:\n{out}");

    // A nested declaration must still see its enclosing scope's constant.
    let nested =
        generate_str("package test.e;\ninterface IFoo { const int X = 5; enum E { A = X, B } }")
            .expect("must generate");
    assert!(nested.contains("r#A = 5,"), "got:\n{nested}");
    assert!(nested.contains("r#B = 6,"), "got:\n{nested}");
}

/// AOSP Rust generates `@JavaOnlyImmutable` normally; only `@JavaOnlyStableParcelable` has no Rust.
#[test]
fn java_only_immutable_keeps_its_fields() {
    let out = generate_str(
        "package im;\n@JavaOnlyImmutable parcelable Bar { String s = \"bar\"; int n = 7; }",
    )
    .expect("must generate");
    assert!(out.contains("pub r#s: String"), "got:\n{out}");
    assert!(out.contains("pub r#n: i32"), "got:\n{out}");

    let u = generate_str("package im;\n@JavaOnlyImmutable union U { int num; String s; }")
        .expect("must generate");
    assert!(u.contains("pub enum r#U"), "got:\n{u}");
}

/// No Rust representation is a diagnostic, not a fieldless struct writing an empty payload.
#[test]
fn unrepresentable_declarations_are_diagnostics() {
    for src in [
        "package a; @JavaOnlyStableParcelable parcelable S { int x; }",
        "package a; parcelable PB cpp_header \"x.h\";",
        "package a; parcelable PB ndk_header \"x.h\";",
    ] {
        assert!(!generate_ok(src), "must be rejected: {src}");
    }
    // ...unless it names a `rust_type`, which *is* representable.
    let out = generate_str(
        "package android.os;\n@JavaOnlyStableParcelable parcelable PB \
         cpp_header \"b.h\" rust_type \"crate::pb::PB\";",
    )
    .expect("a rust_type alias must still generate");
    assert!(out.contains("pub type PB = crate::pb::PB;"), "got:\n{out}");
}

/// An enum reference is its integral value in a binary expression (AOSP `AidlConstantReference`).
#[test]
fn enum_reference_compares_as_its_value() {
    let src = |expr: &str| {
        format!(
            "package a;\n@Backing(type=\"int\") enum Status {{ OK = 0 }}\n\
             interface I {{ const boolean B = {expr}; void f(); }}"
        )
    };
    for expr in ["Status.OK == 0", "0 == Status.OK"] {
        let out = generate_str(&src(expr)).expect("must generate");
        assert!(
            out.contains("pub const r#B: bool = true;"),
            "`{expr}` must fold to true, got:\n{out}"
        );
    }
    let out = generate_str(&src("Status.OK != Status.OK")).expect("must generate");
    assert!(
        out.contains("pub const r#B: bool = false;"),
        "a value must equal itself, got:\n{out}"
    );

    // A float operand must not be truncated to the reference's integer width.
    let d = generate_str(
        "package a;\n@Backing(type=\"int\") enum Status { OK = 1 }\n\
         interface I { const double D = Status.OK + 1.5; void f(); }",
    )
    .expect("must generate");
    assert!(d.contains("pub const r#D: f64 = 2.5f64;"), "got:\n{d}");
}

/// AOSP's grammar takes at most one direction; `in out` must not silently become `out`.
#[test]
fn duplicated_argument_direction_is_rejected() {
    for src in [
        "package a; interface I { void f(in out int[] d); }",
        "package a; interface I { void f(out in int[] d); }",
        "package a; interface I { void f(inout in int[] d); }",
    ] {
        assert!(!generate_ok(src), "must be rejected: {src}");
    }
    assert!(generate_ok(
        "package a; interface I { void f(inout int[] d); }"
    ));
}

/// `self`/`Self`/`super`/`crate` are path keywords with no raw form, so no emit site can hold them.
#[test]
fn unrepresentable_identifiers_are_rejected_everywhere() {
    for src in [
        "package a; interface I { void self(); }",
        "package a; @Backing(type=\"int\") enum E { self = 1 }",
        "package a; interface self { void f(); }",
        "package a; parcelable crate { int x; }",
        "package a; parcelable P { int self; }",
    ] {
        assert!(!generate_ok(src), "must be rejected: {src}");
    }
    // Argument names are emitted as `_arg_<name>`, so any AIDL identifier is representable.
    for src in [
        "package a; interface I { void f(in int crate); }",
        "package a; interface I { void f(in int _); }",
    ] {
        assert!(generate_ok(src), "must be accepted: {src}");
    }
}

/// A keyword package segment's `pub mod` is escaped like `Namespace::relative_mod` references.
#[test]
fn keyword_package_segment_is_escaped() {
    // Module nesting is built by `Builder::generate_all`, not `Generator::document`.
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("keyword_pkg");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("P.aidl"),
        "package com.example.impl; parcelable P { int x; }",
    )
    .unwrap();

    rsbinder_aidl::Builder::new()
        .source(dir.join("P.aidl"))
        .dest_dir(&dir)
        .output("gen.rs")
        .generate()
        .expect("must generate");
    let out = std::fs::read_to_string(dir.join("gen.rs")).expect("output written");
    assert!(out.contains("pub mod r#impl {"), "got:\n{out}");
}

/// A `Default`-less `out` type is `Option<T>` (AOSP `aidl_to_rust.cpp::RustNameOf`); `inout` isn't.
#[test]
fn out_argument_without_default_is_optional() {
    let out =
        generate_str("package a; interface I { void m(out IBinder b); }").expect("must generate");
    assert!(
        out.contains("_arg_b: &mut Option<rsbinder::SIBinder>"),
        "got:\n{out}"
    );
    assert!(
        out.contains("let mut _arg_b: Option<rsbinder::SIBinder> = Default::default();"),
        "got:\n{out}"
    );
}

/// The server passes `&mut` its `inout` local straight in, so it must match the trait signature.
#[test]
fn inout_array_signature_matches_server_local() {
    for (src, ty) in [
        (
            "package a; interface I { void m(inout ParcelFileDescriptor[] p); }",
            "Vec<rsbinder::ParcelFileDescriptor>",
        ),
        (
            "package a; interface I { void m(inout IBinder[] p); }",
            "Vec<rsbinder::SIBinder>",
        ),
        (
            "package a; interface I { void m(inout @nullable int[] p); }",
            "Option<Vec<i32>>",
        ),
        (
            "package a; interface I { void m(inout @nullable String[] p); }",
            "Option<Vec<Option<String>>>",
        ),
    ] {
        let out = generate_str(src).expect("must generate");
        assert!(
            out.contains(&format!("_arg_p: &mut {ty}")),
            "trait signature must be `&mut {ty}`, got:\n{out}"
        );
        assert!(
            out.contains(&format!("let mut _arg_p: {ty} = _reader.read()?;")),
            "server local must be `{ty}`, got:\n{out}"
        );
    }
}

/// A fixed-size array constant's literal does not coerce to a slice reference in a const.
#[test]
fn fixed_size_const_array_is_a_value_type() {
    let out = generate_str("package a; interface I { const int[3] X = {1,2,3}; void f(); }")
        .expect("must generate");
    assert!(
        out.contains("pub const r#X: [i32; 3] = [1,2,3,];"),
        "got:\n{out}"
    );
    // The variable-length form stays a slice.
    let v = generate_str("package a; interface I { const int[] X = {1,2,3}; void f(); }")
        .expect("must generate");
    assert!(
        v.contains("pub const r#X: &[i32] = &[1,2,3,];"),
        "got:\n{v}"
    );
}

/// Out-of-range discriminants are rejected as in AOSP; rustc would deny the emitted literal.
#[test]
fn enum_discriminant_must_fit_its_backing_type() {
    assert!(!generate_ok(
        "package a; @Backing(type=\"byte\") enum E { A = 200 }"
    ));
    assert!(!generate_ok(
        "package a; @Backing(type=\"int\") enum E { A = 4294967296 }"
    ));
    assert!(generate_ok(
        "package a; @Backing(type=\"byte\") enum E { A = 127, B = -128 }"
    ));
}

/// Two field names mapping to one UpperCamel variant would be `E0428`, so it is a diagnostic.
#[test]
fn colliding_union_variant_names_are_a_diagnostic() {
    assert!(!generate_ok(
        "package a; union U { int my_field; int myField; }"
    ));
    assert!(generate_ok(
        "package a; union U { int my_field; int other; }"
    ));
}

/// The templates always emit `#[derive(Debug)]`; repeating it from `@RustDerive` is `E0119`.
#[test]
fn rust_derive_does_not_duplicate_debug() {
    let out =
        generate_str("package a; @RustDerive(Debug=true, Clone=true) parcelable D { int a; }")
            .expect("must generate");
    assert_eq!(
        out.matches("Debug").count(),
        1,
        "Debug must be derived exactly once, got:\n{out}"
    );
    assert!(out.contains("#[derive(Clone)]"), "got:\n{out}");
}

/// A `rust_type` declaration's name is escaped like every other declaration path.
#[test]
fn rust_type_declaration_name_is_escaped() {
    let out = generate_str("package a; parcelable type rust_type \"i32\";").expect("must generate");
    assert!(out.contains("pub mod r#type {"), "got:\n{out}");
    assert!(out.contains("pub type r#type = i32;"), "got:\n{out}");
}

/// AOSP `ValueString`: a char literal initializes only a `char`.
#[test]
fn char_literal_initializes_only_char() {
    for src in [
        "parcelable P { boolean b = '\\0'; }",
        "parcelable P { int i = 'a'; }",
        "interface I { const long L = 'a'; }",
    ] {
        assert!(!generate_ok(src), "must be rejected: {src}");
    }
    assert!(generate_ok(
        "parcelable P { char c = 'a'; char[] cs = {'a'}; }"
    ));
}

/// rustc denies a raw bidi control in a literal (`text_direction_codepoint_in_literal`).
#[test]
fn bidi_controls_in_literals_are_escaped() {
    let out = generate_str("parcelable P { String s = \"\u{202E}abc\"; char c = '\u{2066}'; }")
        .expect("generates");
    assert!(out.contains("\\u{202e}abc"), "{out}");
    assert!(out.contains("'\\u{2066}'"), "{out}");
    assert!(
        !out.contains('\u{202E}') && !out.contains('\u{2066}'),
        "{out}"
    );
}

/// A dotted `rust_type` name would emit `pub mod Outer.Inner`, a rustc syntax error.
#[test]
fn qualified_rust_type_parcelable_name_is_rejected() {
    assert!(!generate_ok(
        "package a; parcelable Outer.Inner rust_type \"i32\";"
    ));
}

/// `Path::is_dir()` follows symlinks, so a link cycle yields ever-deeper distinct paths.
#[cfg(unix)]
#[test]
fn symlink_cycle_in_a_source_directory_terminates() {
    use std::sync::mpsc;
    use std::time::Duration;

    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("symlink_cycle");
    let _ = std::fs::remove_dir_all(&dir);
    let aidl = dir.join("aidl");
    std::fs::create_dir_all(&aidl).unwrap();
    std::fs::write(aidl.join("P.aidl"), "package a; parcelable P { int x; }").unwrap();
    std::os::unix::fs::symlink(".", aidl.join("loop")).unwrap();

    let (tx, rx) = mpsc::channel();
    let out = dir.join("out");
    std::thread::spawn(move || {
        let r = rsbinder_aidl::Builder::new()
            .source(&aidl)
            .dest_dir(&out)
            .output("gen.rs")
            .generate();
        let _ = tx.send(r.is_ok());
    });

    match rx.recv_timeout(Duration::from_secs(20)) {
        Ok(ok) => assert!(ok, "generation over a symlinked directory must succeed"),
        Err(_) => {
            // Still walking; `abort` skips libtest's capture flush, so write stderr directly.
            use std::io::Write;
            let _ = std::io::stderr()
                .write_all(b"symlink cycle test: the directory walk did not terminate; aborting\n");
            std::process::abort();
        }
    }
}

/// The output directory is created whether `dest_dir` is missing or `output` names a subdir.
#[test]
fn output_directory_is_created_on_demand() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("out_dir_create");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("P.aidl");
    std::fs::write(&src, "package a; parcelable P { int x; }").unwrap();

    rsbinder_aidl::Builder::new()
        .source(&src)
        .dest_dir(dir.join("does/not/exist"))
        .output("gen.rs")
        .generate()
        .expect("a missing dest_dir must be created");

    rsbinder_aidl::Builder::new()
        .source(&src)
        .dest_dir(&dir)
        .output("nested/gen.rs")
        .generate()
        .expect("a nested output path must be created");
    assert!(dir.join("nested/gen.rs").is_file());
}

/// The parser resets in `generate()`; `P`'s unimported `T` resolves only if declarations leak.
#[test]
fn a_second_builder_does_not_inherit_the_first_declarations() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("two_builders");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("a"), root.join("b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join("T.aidl"), "package p; parcelable T { int x; }").unwrap();
    std::fs::write(b.join("P.aidl"), "package p; parcelable P { T item; }").unwrap();

    // Both builders are constructed before either generates.
    let first = rsbinder_aidl::Builder::new()
        .source(a.join("T.aidl"))
        .dest_dir(a.join("out"))
        .output("gen.rs");
    let second = rsbinder_aidl::Builder::new()
        .source(b.join("P.aidl"))
        .dest_dir(b.join("out"))
        .output("gen.rs");
    first.generate().expect("must generate");
    let err = second
        .generate()
        .expect_err("`T` is neither imported nor declared by the second builder");
    assert!(err.to_string().contains("unknown type 'T'"), "got: {err:?}");
}

/// A fixed-size `String` constant is a `[&str; N]`, matching the variable-length `&[&str]`.
#[test]
fn fixed_size_string_array_constant_is_a_str_array() {
    let out = generate_str(
        "package a; interface I { const String[2] X = {\"a\", \"b\"}; const int[2] N = {1, 2}; void p(); }",
    )
    .expect("must generate");
    assert!(
        out.contains("pub const r#X: [&str; 2] = [\"a\",\"b\",];"),
        "got:\n{out}"
    );
    assert!(
        out.contains("pub const r#N: [i32; 2] = [1,2,];"),
        "got:\n{out}"
    );
}

/// An enum reference folds at its `@Backing` width; naming an `int` enum member does not widen.
#[test]
fn enum_reference_promotes_at_its_backing_width() {
    let out = generate_str(
        "package a; @Backing(type=\"int\") enum F { BIT = 1 } \
         @Backing(type=\"long\") enum L { BIT = 1 } \
         interface I { const int SIGN = F.BIT << 31; const long WIDE = L.BIT << 40; void p(); }",
    )
    .expect("must generate");
    assert!(
        out.contains("pub const r#SIGN: i32 = -2147483648;"),
        "got:\n{out}"
    );
    assert!(
        out.contains("pub const r#WIDE: i64 = 1099511627776;"),
        "got:\n{out}"
    );
    // The same shift written as a literal must fold identically.
    let literal = generate_str("package a; interface I { const int SIGN = 1 << 31; void p(); }")
        .expect("must generate");
    assert!(
        literal.contains("pub const r#SIGN: i32 = -2147483648;"),
        "got:\n{literal}"
    );
}

/// `-E.A`, `~E.A` and `!E.A` all fold through the reference's integral value.
#[test]
fn unary_operators_apply_to_enum_references() {
    let out = generate_str(
        "package a; @Backing(type=\"int\") enum E { A = 1 } \
         interface I { const int NEG = -E.A; const int INV = ~E.A; const int NOT = !E.A; void p(); }",
    )
    .expect("must generate");
    assert!(out.contains("pub const r#NEG: i32 = -1;"), "got:\n{out}");
    assert!(out.contains("pub const r#INV: i32 = -2;"), "got:\n{out}");
    assert!(out.contains("pub const r#NOT: i32 = 0;"), "got:\n{out}");
}

/// Package segments become `mod` names, so path keywords are refused there too, per dotted segment.
#[test]
fn unrepresentable_keywords_are_rejected_in_package_and_qualified_names() {
    for src in [
        "package com.self; interface I { void p(); }",
        "package com.example.crate; interface I { void p(); }",
        "package a; parcelable b.super.P { int x; }",
    ] {
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", src);
        let err = rsbinder_aidl::parse_document(&ctx).expect_err(src);
        let rsbinder_aidl::AidlError::Parse(pe) = &err else {
            panic!("expected a ParseError for {src:?}, got: {err:?}");
        };
        assert!(
            pe.message
                .contains("not representable as a Rust raw identifier"),
            "expected the keyword diagnostic for {src:?}, got: {}",
            pe.message
        );
    }
    assert!(generate_ok(
        "package com.example.impl; interface I { void p(); }"
    ));
}

/// Lint allowances are inner attributes: a package-less document has no outer module to hold them.
#[test]
fn every_generated_module_carries_the_lint_allowances() {
    let out =
        generate_str("parcelable A { int x; } parcelable B { int y; }").expect("must generate");
    assert_eq!(
        out.matches("#![allow(clippy::all, unused_imports,").count(),
        2,
        "got:\n{out}"
    );

    // The package-less path through `Builder`: no wrapping module, only inner attributes.
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("package_less");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("INoPkg.aidl"),
        "interface INoPkg { void ping(); }",
    )
    .unwrap();
    rsbinder_aidl::Builder::new()
        .source(root.join("INoPkg.aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("must generate");
    let file = std::fs::read_to_string(root.join("out/gen.rs")).expect("output written");
    assert!(file.starts_with("pub mod INoPkg {"), "got:\n{file}");
    assert!(
        file.contains("#![allow(clippy::all, unused_imports,"),
        "got:\n{file}"
    );
}

/// An unset non-nullable `Option` out argument (fd arrays too) fails the call, never writes null.
#[test]
fn non_nullable_out_arguments_reject_an_unset_value() {
    let out =
        generate_str("package a; interface I { void f(out IBinder b, out @nullable IBinder n); }")
            .expect("must generate");
    assert!(
        out.contains("let _arg_b = _arg_b.as_ref().ok_or(rsbinder::StatusCode::UnexpectedNull)?;"),
        "got:\n{out}"
    );
    assert!(
        !out.contains("let _arg_n = _arg_n"),
        "@nullable must stay nullable, got:\n{out}"
    );

    let fds = generate_str(
        "package a; interface I { void f(out ParcelFileDescriptor[3] p, out ParcelFileDescriptor[] q); }",
    )
    .expect("must generate");
    assert_eq!(
        fds.matches("iter().any(Option::is_none)").count(),
        2,
        "both fixed and variable out fd arrays are guarded, got:\n{fds}"
    );
}

/// AOSP `UsesOptionInNullableVector` applies to fixed-size arrays too (primitive/enum stay bare).
#[test]
fn nullable_fixed_array_follows_the_vector_element_rule() {
    let out = generate_str(
        "package a; @Backing(type=\"int\") enum E { A = 1 } \
         interface I { void f(out @nullable int[3] i, out @nullable E[3] e, out @nullable String[3] s); }",
    )
    .expect("must generate");
    for expected in [
        "_arg_i: &mut Option<[i32; 3]>",
        "_arg_e: &mut Option<[super::E::E; 3]>",
        "_arg_s: &mut Option<[Option<String>; 3]>",
    ] {
        assert!(out.contains(expected), "expected `{expected}`, got:\n{out}");
    }
}

/// A `@nullable String` array constant's type has the element `Option` its initializer emits.
#[test]
fn nullable_string_array_constant_type_matches_its_initializer() {
    let out = generate_str(
        "package a; interface I { const @nullable String[2] X = {\"a\",\"b\"}; \
         const @nullable String[] Y = {\"a\"}; void p(); }",
    )
    .expect("must generate");
    assert!(
        out.contains(
            "pub const r#X: Option<[Option<&str>; 2]> = Some([Some(\"a\"),Some(\"b\"),]);"
        ),
        "got:\n{out}"
    );
    assert!(
        out.contains("pub const r#Y: Option<&[Option<&str>]> = Some(&[Some(\"a\"),]);"),
        "got:\n{out}"
    );
}

/// The pre-parse guard counts `<` as nesting only once a `>` closes it, so comparisons pass.
#[test]
fn comparison_operators_do_not_trip_the_generic_guard() {
    let cmp = format!(
        "package p; parcelable P {{ int[] flags = {{ {}0<1 }}; }}",
        "0<1, ".repeat(20)
    );
    assert!(
        generate_ok(&cmp),
        "20 comparisons in one statement must parse"
    );

    let deep = format!(
        "package a; parcelable P {{ {}int{} x; }}",
        "List<".repeat(15),
        ">".repeat(15)
    );
    let ctx = rsbinder_aidl::SourceContext::new("t.aidl", &deep);
    let err = rsbinder_aidl::parse_document(&ctx).expect_err("15 nested generics must be refused");
    let rsbinder_aidl::AidlError::Parse(pe) = &err else {
        panic!("expected a ParseError, got: {err:?}");
    };
    assert!(
        pe.message.contains("generic types are nested too deeply"),
        "got: {}",
        pe.message
    );
}

/// Unfit enum references keep their value: `decl_enum`'s range check runs only on its own output.
#[test]
fn an_out_of_range_enum_reference_is_not_truncated() {
    let e = rsbinder_aidl::SourceContext::new(
        "e.aidl",
        "package a; @Backing(type=\"int\") enum E { A = 34359738367 }",
    );
    rsbinder_aidl::parse_document(&e).expect("the enum document parses");
    let i = rsbinder_aidl::SourceContext::new(
        "i.aidl",
        "package a; interface I { const long X = E.A & -1; void p(); }",
    );
    let doc = rsbinder_aidl::parse_document(&i).expect("the interface document parses");
    let (_, out) = rsbinder_aidl::Generator::new(false, false)
        .document(&doc)
        .expect("must generate");
    assert!(
        out.contains("pub const r#X: i64 = 34359738367;"),
        "got:\n{out}"
    );
}

/// AOSP's seven-trait `@RustDerive` schema; always-emitted impls (`Debug`, `Default`) are dropped.
#[test]
fn rust_derive_accepts_only_the_aosp_schema() {
    let out =
        generate_str("package a; @RustDerive(Default=true, Clone=true) parcelable P { int x; }")
            .expect("must generate");
    assert!(!out.contains("#[derive(Default"), "got:\n{out}");
    assert!(!out.contains("Default,"), "got:\n{out}");
    assert!(out.contains("#[derive(Clone)]"), "got:\n{out}");
    assert!(out.contains("impl Default for P"), "got:\n{out}");
}

/// `""` (from `strip_package`) is the cwd: dedup with `.`, never emit a bare `rerun-if-changed=`.
#[test]
fn an_empty_include_path_is_the_working_directory() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("empty_include");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("P.aidl"), "package a; parcelable P { int x; }").unwrap();

    let deps = rsbinder_aidl::Builder::new()
        .include_dir(std::path::PathBuf::from(""))
        .include_dir(std::path::PathBuf::from("."))
        .source(root.join("P.aidl"))
        .collect_aidl_dependencies()
        .expect("collect_aidl_dependencies");
    assert!(
        !deps.iter().any(|d| d.as_os_str().is_empty()),
        "an empty dependency path reaches cargo as a bare `rerun-if-changed=`: {deps:?}"
    );
    assert_eq!(
        deps.iter()
            .filter(|d| d.as_os_str() == "." || d.as_os_str().is_empty())
            .count(),
        1,
        "`\"\"` and `\".\"` are one directory: {deps:?}"
    );
}

/// Two spellings of one include dir (symlink/target, `./aidl`/absolute) are no `AmbiguousImport`.
#[cfg(unix)]
#[test]
fn one_include_directory_under_two_spellings_is_not_ambiguous() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("include_spellings");
    let _ = std::fs::remove_dir_all(&root);
    let aidl = root.join("aidl/hello");
    std::fs::create_dir_all(&aidl).unwrap();
    std::fs::write(
        aidl.join("IHello.aidl"),
        "package hello; import hello.IWorld; interface IHello { IWorld get(); }",
    )
    .unwrap();
    std::fs::write(
        aidl.join("IWorld.aidl"),
        "package hello; interface IWorld { void ping(); }",
    )
    .unwrap();
    std::os::unix::fs::symlink(root.join("aidl"), root.join("link")).unwrap();

    // `include_dir` names the target; the source's package-derived include dir is the link.
    rsbinder_aidl::Builder::new()
        .include_dir(root.join("aidl"))
        .source(root.join("link/hello/IHello.aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("one directory under two names must not be an ambiguous import");
}

/// An empty hash is falsy to Tera and would silently drop `getInterfaceHash()` (cf. `version`).
#[test]
#[should_panic(expected = "the hash must be non-empty")]
fn empty_interface_hash_is_rejected() {
    // `hash` checks the hash string; the source only needs to be a file path.
    let _ = rsbinder_aidl::Builder::new().source("I.aidl").hash("");
}

/// `import a.IFoo;` makes `IFoo.BAR` mean `a.IFoo.BAR`; only a second package exercises it.
#[test]
fn imported_interface_constant_resolves_across_packages() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("import_const");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("aidl/a"), root.join("aidl/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    // `IBaz` shadows `IFoo`'s `BASE`, so folding `BAR` in the wrong scope yields 101.
    std::fs::write(
        a.join("IFoo.aidl"),
        "package a; interface IFoo { const int BASE = 10; const int BAR = BASE + 1; }",
    )
    .unwrap();
    std::fs::write(
        b.join("IBaz.aidl"),
        "package b; import a.IFoo; interface IBaz { const int BASE = 100; const int X = IFoo.BAR; const int Y = a.IFoo.BAR; }",
    )
    .unwrap();

    rsbinder_aidl::Builder::new()
        .source(root.join("aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("`IFoo.BAR` resolves through the import");
    let out = std::fs::read_to_string(root.join("out/gen.rs")).unwrap();
    assert!(
        out.contains("pub const r#X: i32 = 11;"),
        "the imported constant folds in its owner's scope: {out}"
    );
    assert!(
        out.contains("pub const r#Y: i32 = 11;"),
        "so does the fully-qualified reference: {out}"
    );
}

/// An `import` outranks a same-named package declaration (AOSP `AidlDocument::ResolveName`).
#[test]
fn an_import_outranks_a_same_named_package_declaration() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("import_shadow");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("aidl/a"), root.join("aidl/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(
        a.join("IFoo.aidl"),
        "package a; interface IFoo { const int BAR = 7; }",
    )
    .unwrap();
    std::fs::write(
        b.join("IFoo.aidl"),
        "package b; interface IFoo { const int BAR = 2; }",
    )
    .unwrap();
    std::fs::write(
        b.join("IBaz.aidl"),
        "package b; import a.IFoo; interface IBaz { const int X = IFoo.BAR; }",
    )
    .unwrap();

    rsbinder_aidl::Builder::new()
        .source(root.join("aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("generates");
    let out = std::fs::read_to_string(root.join("out/gen.rs")).unwrap();
    assert!(
        out.contains("pub const r#X: i32 = 7;"),
        "`IFoo.BAR` must be the imported `a.IFoo`, not `b.IFoo`: {out}"
    );
}

/// The out-fd-array null guard adds one `.flatten()` per extra dimension to reach every cell.
#[test]
fn out_fd_array_null_guard_flattens_nested_dimensions() {
    let one = generate_str("package a; interface I { void f(out ParcelFileDescriptor[2] fds); }")
        .expect("1-D generates");
    assert!(
        one.contains("fds.iter().any(Option::is_none)"),
        "the 1-D guard stays: {one}"
    );
    let two =
        generate_str("package a; interface I { void f(out ParcelFileDescriptor[2][3] fds); }")
            .expect("2-D generates");
    assert!(
        two.contains("fds.iter().flatten().any(Option::is_none)"),
        "a 2-D array is guarded through one flatten: {two}"
    );
}

/// A `@RustDerive` name outside AOSP's schema is a spanned error, not a silently missing derive.
#[test]
fn unknown_rust_derive_parameter_is_an_error() {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        "package a; @RustDerive(Cloen=true) parcelable P { int x; }",
    );
    let err = rsbinder_aidl::parse_document(&ctx).expect_err("a misspelt derive must not parse");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown @RustDerive parameter 'Cloen'"),
        "got: {msg}"
    );
    assert!(
        generate_ok("package a; @RustDerive(Clone=true, PartialEq=true) parcelable P { int x; }"),
        "the schema itself still parses"
    );
}

/// AOSP `CheckValid` runs `ValueString(boolean)`: only a bool or an integer is a derive flag.
#[test]
fn rust_derive_value_must_be_a_boolean() {
    for src in [
        "package a; @RustDerive(Clone='\\0') parcelable P { int x; }",
        "package a; @RustDerive(Clone=\"true\") parcelable P { int x; }",
        "package a; @RustDerive(Clone=1.0) parcelable P { int x; }",
    ] {
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", src);
        let err = rsbinder_aidl::parse_document(&ctx).expect_err(src);
        assert!(
            err.to_string()
                .contains("Invalid value for parameter Clone on annotation RustDerive."),
            "{src}: {err}"
        );
    }
    assert!(generate_ok(
        "package a; @RustDerive(Clone=1, PartialEq=false) parcelable P { int x; }"
    ));
}

/// AOSP `ConstReferenceFinder`: an annotation value naming a constant is refused in any order.
#[test]
fn annotation_value_reference_is_rejected_in_any_source_order() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("annotation_reference");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("a/p"), root.join("b/q"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    let consts = a.join("IConsts.aidl");
    std::fs::write(
        &consts,
        "package p; interface IConsts { const int YES = 1; }",
    )
    .unwrap();
    let user = b.join("P.aidl");
    std::fs::write(
        &user,
        "package q; @RustDerive(Clone = p.IConsts.YES) parcelable P { int x; }",
    )
    .unwrap();
    for (first, second) in [(&consts, &user), (&user, &consts)] {
        let err = rsbinder_aidl::Builder::new()
            .source(first)
            .source(second)
            .collect_aidl_dependencies()
            .expect_err("a reference in an annotation value");
        assert!(
            format!("{err:?}").contains("contains reference to p.IConsts.YES"),
            "{first:?} then {second:?}: {err:?}"
        );
    }
}

/// A phantom owner's member stays unresolved: no lexical or parent fallback may supply it.
#[test]
fn dotted_constant_with_a_phantom_owner_is_unresolved() {
    assert!(
        !generate_ok("package b; interface IBaz { const int BAR = 1; const int X = Nope.BAR; }"),
        "`Nope.BAR` must not resolve to the current declaration's `BAR`"
    );
    assert!(
        !generate_ok(
            "package b; parcelable Outer { const int X = 5; const int Y = Outer.Nope.X; }"
        ),
        "`Outer.Nope.X` must not resolve to `Outer.X`"
    );

    // Through an import, a member the owner does not declare stays unresolved.
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("import_phantom");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b) = (root.join("aidl/a"), root.join("aidl/b"));
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(
        a.join("IFoo.aidl"),
        "package a; interface IFoo { const int BAR = 7; }",
    )
    .unwrap();
    std::fs::write(
        b.join("IBaz.aidl"),
        "package b; import a.IFoo; interface IBaz { const int MISSING = 1; const int OK = IFoo.BAR; const int X = IFoo.MISSING; }",
    )
    .unwrap();
    let err = rsbinder_aidl::Builder::new()
        .source(root.join("aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect_err("`IFoo.MISSING` names nothing in `a.IFoo`");
    let msg = err.to_string();
    assert!(
        msg.contains("IFoo.MISSING") && !msg.contains("IFoo.BAR"),
        "only the phantom member is diagnosed: {msg}"
    );
}

/// A qualified constant names a *direct* member: `Outer.X` is not `Outer.Inner.X`.
#[test]
fn nested_declaration_constants_are_not_members_of_the_outer() {
    assert!(
        !generate_ok(
            "package a; parcelable Outer { const int BASE = 1; parcelable Inner { const int BASE = 10; const int X = BASE + 1; } } \
             interface IBaz { const int P = Outer.X; }"
        ),
        "`Outer.X` must not reach `Outer.Inner.X`"
    );
    let out = generate_str(
        "package a; parcelable Outer { parcelable Inner { const int BASE = 10; const int X = BASE + 1; } const int BASE = 1; const int Y = BASE + 1; }",
    )
    .expect("generates");
    assert!(out.contains("pub const r#X: i32 = 11;"), "got: {out}");
    assert!(
        out.contains("pub const r#Y: i32 = 2;"),
        "`Outer.Y` folds against `Outer.BASE`, not `Inner.BASE`: {out}"
    );
}

/// In a `c`→`a`→`b`→`a` diamond every constant folds in its owner's scope, even re-entrantly.
#[test]
fn diamond_constant_references_fold_in_their_owners_scope() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("const_diamond");
    let _ = std::fs::remove_dir_all(&root);
    let (a, b, c) = (
        root.join("aidl/a"),
        root.join("aidl/b"),
        root.join("aidl/c"),
    );
    for d in [&a, &b, &c] {
        std::fs::create_dir_all(d).unwrap();
    }
    std::fs::write(
        a.join("IFoo.aidl"),
        "package a; import b.IBaz; interface IFoo { const int BASE = 7; const int C2 = BASE; const int C1 = IBaz.X + 1; }",
    )
    .unwrap();
    std::fs::write(
        b.join("IBaz.aidl"),
        "package b; import a.IFoo; interface IBaz { const int BASE = 99; const int X = IFoo.C2; }",
    )
    .unwrap();
    std::fs::write(
        c.join("IQux.aidl"),
        "package c; import a.IFoo; interface IQux { const int Y = IFoo.C1; }",
    )
    .unwrap();

    rsbinder_aidl::Builder::new()
        .source(root.join("aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("generates");
    let out = std::fs::read_to_string(root.join("out/gen.rs")).unwrap();
    for pin in [
        "pub const r#C1: i32 = 8;",
        "pub const r#X: i32 = 7;",
        "pub const r#Y: i32 = 8;",
    ] {
        assert!(out.contains(pin), "missing `{pin}`: {out}");
    }
}

/// Debug text of the parse or generation error; panics when both succeed.
fn generate_err(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let result = rsbinder_aidl::parse_document(&ctx)
        .and_then(|doc| rsbinder_aidl::Generator::new(false, false).document(&doc));
    match result {
        Ok((_, out)) => panic!("expected an error for {input:?}, got:\n{out}"),
        Err(err) => format!("{err:?}"),
    }
}

/// AOSP lexes longest-match, so `trueCount` and `onewayResult` are identifiers.
#[test]
fn keyword_prefixed_identifiers_are_identifiers() {
    let out = generate_str("interface I { const int trueCount = 1; const int X = trueCount; }")
        .expect("`trueCount` is a constant name");
    assert!(out.contains("pub const r#X: i32 = 1;"), "{out}");
    let out =
        generate_str("parcelable onewayResult { int x; } interface I { onewayResult get(); }")
            .expect("`onewayResult` is a type name");
    assert!(
        out.contains("fn r#get(&self) -> rsbinder::BinderResult<"),
        "{out}"
    );
    assert!(
        !out.contains("FLAG_ONEWAY"),
        "`get` must not be oneway:\n{out}"
    );
}

/// AOSP grammar separates arguments and annotation parameters with commas.
#[test]
fn missing_list_commas_are_parse_errors() {
    generate_err("interface I { void f(int a int b); }");
    generate_err("@JavaDerive(equals=true toString=true) parcelable P { int x; }");
}

/// A qualifier that names no declaration must not fall back to the enum being declared.
#[test]
fn unknown_enum_qualifier_is_not_the_current_enum() {
    let err = generate_err("enum E { X = 5, Y = Nope.X }");
    assert!(
        err.contains("invalid discriminant") || err.contains("Nope"),
        "{err}"
    );
}

/// AOSP refuses unstructured parcelables for the Rust backend.
#[test]
fn unstructured_parcelable_is_rejected() {
    let err = generate_err("parcelable Foo;");
    assert!(err.contains("is unstructured"), "{err}");
    assert!(
        generate_ok("parcelable Foo {}"),
        "an empty structured parcelable still generates"
    );
}

/// An enum reference converts to the target like any other value (AOSP `ValueString`).
#[test]
fn enum_reference_converts_to_a_non_enum_target() {
    let out = generate_str(
        "@Backing(type=\"int\") enum E { A = 1, BIG = 300 } parcelable P { boolean b = E.A; }",
    )
    .expect("an int enum value initializes a boolean");
    assert!(out.contains("r#b: true"), "{out}");
    generate_err(
        "@Backing(type=\"int\") enum E { A = 1, BIG = 300 } interface I { const byte B = E.BIG; }",
    );
    generate_err("enum E { A } parcelable Q { int x; } parcelable P { Q q = E.A; }");
}

/// AOSP `ValueString`: a defined type that is not an enum takes no constant value at all.
#[test]
fn non_enum_defined_type_takes_no_constant() {
    for src in [
        "parcelable Q { int x; } parcelable P { Q q = 5; }",
        "interface IFoo { void m(); } parcelable P { IFoo f = \"x\"; }",
        "enum E { A } parcelable Q { int x; } parcelable P { Q[] qs = {E.A}; }",
    ] {
        generate_err(src);
    }
}

/// AOSP `ValueString`: String takes only a string, integral targets never take a float.
#[test]
fn const_conversions_reject_mismatched_kinds() {
    for src in [
        "interface I { const String S = 5; }",
        "interface I { const String S = true; }",
        "interface I { const String[] N = { UNDEFINED }; }",
        "interface I { const int X = 1.5; }",
        "interface I { const long L = 1e19; }",
        "parcelable P { boolean b = 0.5; }",
    ] {
        generate_err(src);
    }
}

/// AOSP `AidlUnaryConstExpression::IsCompatibleType`: no char; bool per `OverflowGuard<bool>`.
#[test]
fn unary_operators_follow_aosp_on_char_and_bool() {
    generate_err("interface I { const char A = 'a'; const char B = -A; }");
    generate_err("interface I { const char A = 'a'; const char B = ~A; }");
    generate_err("interface I { const char A = 'a'; const char B = +A; }");
    generate_err("interface I { const char A = 'a'; const boolean B = !A; }");
    generate_err("interface I { const int A = !1.5; }");
    // `AreCompatibleOperandTypes` has no CHARACTER case, whatever the promoted type.
    generate_err("interface I { const char A = 'a'; const float F = A + 1.5; }");
    generate_err("interface I { const char A = 'a'; const boolean B = A == 97; }");
    let out = generate_str("interface I { const float F = +1.5; const float G = -1.5; }")
        .expect("unary +/- apply to a float");
    assert!(out.contains("r#G"), "{out}");
    generate_err("interface I { const int A = -(1 == 0); }");
    generate_err("interface I { const int A = ~(1 == 1); }");
    let out = generate_str("interface I { const int A = -(1 == 1); }").expect("-true is true");
    assert!(out.contains("pub const r#A: i32 = 1;"), "{out}");
    // `!` on an integer keeps `T` (`OverflowGuard<T>::operator!`), so a further unary is integral.
    let out = generate_str(
        "interface I { const int A = -!0; const int B = -!5; const int C = ~!0; \
         const long D = -!0L; const boolean E = !5; const int F = ~!1000; }",
    )
    .expect("! on an integer stays integral");
    for want in [
        "pub const r#A: i32 = -1;",
        "pub const r#B: i32 = 0;",
        "pub const r#C: i32 = -2;",
        "pub const r#D: i64 = -1;",
        "pub const r#E: bool = false;",
        "pub const r#F: i32 = -1;",
    ] {
        assert!(out.contains(want), "missing `{want}`:\n{out}");
    }
}

/// A reference resolves against constants only, never a field default.
#[test]
fn field_default_is_not_a_constant() {
    generate_err("parcelable P { int x = 5; int y = x; }");
}

/// AOSP `Parser::CheckValidTypeName`.
#[test]
fn qualified_structured_type_name_is_rejected() {
    for src in [
        "package a; parcelable b.Foo { int x; }",
        "package a; interface b.IFoo { void m(); }",
        "package a; enum b.E { A }",
        "package a; union b.U { int x; }",
    ] {
        assert!(generate_err(src).contains("can't be qualified"), "{src}");
    }
}

/// AOSP `AidlInterface::CheckValid` reserves the meta-method signatures.
#[test]
fn reserved_meta_methods_are_rejected() {
    for src in [
        "interface I { int getInterfaceVersion(); }",
        "interface I { String getInterfaceHash(); }",
        "interface I { String getTransactionName(int code); }",
        "interface I { IBinder asBinder(); }",
    ] {
        assert!(generate_err(src).contains("reserved"), "{src}");
    }
    assert!(generate_ok(
        "interface I { void getTransactionName(String s); }"
    ));
}

/// Nested enum array literals are validated per dimension; the arity must match every dimension.
#[test]
fn multi_dimensional_array_defaults() {
    let out =
        generate_str("enum E { A, B } parcelable P { E[2][2] m = {{E.A, E.B}, {E.A, E.B}}; }")
            .expect("a 2-D enum array default generates");
    assert!(out.contains("E::B"), "{out}");
    generate_err("parcelable P { int[2][2] x = {1, 2}; }");
    generate_err("parcelable P { int[2] x = {{1}, {2}}; }");
    // Variable-length dims and `List<T>` are rank-checked too (else `vec![vec![..]]` for a Vec).
    generate_err("parcelable P { int[] x = {{1}, {2}}; }");
    generate_err("enum E { A } parcelable P { E[] e = {{E.A}}; }");
    generate_err("parcelable P { List<String> l = {{\"a\"}}; }");
    assert!(generate_ok(
        "enum E { A } parcelable P { E[] e = {E.A}; int[] x = {1, 2}; }"
    ));
}

/// Enum paths in defaults are `r#`-escaped like every other generated path.
#[test]
fn keyword_enum_member_default_is_escaped() {
    let out = generate_str("enum Op { move, copy } parcelable P { Op o = Op.move; Op d; }")
        .expect("generates");
    assert!(out.contains("Op::r#move"), "{out}");
    assert!(!out.contains("Op::move"), "{out}");
}

/// Constant lookup stops at the package boundary of the referencing declaration.
#[test]
fn unqualified_constant_does_not_resolve_through_a_package_segment() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("const_package_floor");
    let _ = std::fs::remove_dir_all(&root);
    let (a, ab) = (root.join("aidl/a"), root.join("aidl/a/b"));
    std::fs::create_dir_all(&ab).unwrap();
    std::fs::write(
        a.join("b.aidl"),
        "package a; interface b { const int X = 1; }",
    )
    .unwrap();
    std::fs::write(
        ab.join("I.aidl"),
        "package a.b; interface I { const int Y = X; }",
    )
    .unwrap();
    let result = rsbinder_aidl::Builder::new()
        .source(root.join("aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate();
    assert!(
        result.is_err(),
        "`X` must not resolve to interface `a.b`'s constant"
    );
}

/// `import p.IOuter.Inner;` finds `p/IOuter.aidl` (AOSP `ImportResolver::FindImportFile`).
#[test]
fn nested_type_import_resolves_to_the_enclosing_file() {
    let root = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("nested_import");
    let _ = std::fs::remove_dir_all(&root);
    let p = root.join("aidl/p");
    std::fs::create_dir_all(&p).unwrap();
    std::fs::write(
        p.join("IOuter.aidl"),
        "package p; interface IOuter { parcelable Inner { int x; } }",
    )
    .unwrap();
    std::fs::write(
        p.join("IFoo.aidl"),
        "package p; import p.IOuter.Inner; interface IFoo { Inner get(); }",
    )
    .unwrap();

    // Only `IFoo` is a source: `IOuter.aidl` is reachable through the import alone.
    rsbinder_aidl::Builder::new()
        .source(p.join("IFoo.aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("a nested-type import resolves to its enclosing file");
    let out = std::fs::read_to_string(root.join("out/gen.rs")).unwrap();
    assert!(
        out.contains("BinderResult<super::IOuter::Inner::Inner>"),
        "`Inner` names the nested type: {out}"
    );
}

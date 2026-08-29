// SPDX-License-Identifier: Apache-2.0
//
// Regression tests for codegen defects found in the full-source review.
// Each case is parseable input that must surface as a recoverable error (or
// compute without panicking) rather than aborting the AIDL compiler — the
// project's "no panic on user input" invariant.

/// Returns `true` only when BOTH parsing and code generation succeed.
fn generate_ok(input: &str) -> bool {
    generate_str(input).is_some()
}

/// Returns the generated Rust source (the `.1` of `Generator::document`) when
/// parse + generation both succeed. NOTE: the generator does not type-check its
/// output, so this succeeding does not prove the emitted Rust *compiles* — use
/// it to assert on the emitted text directly.
fn generate_str(input: &str) -> Option<String> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).ok()?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).ok().map(|(_, rust)| rust)
}

/// A `List<T[]>` (list-of-array) is grammar-valid and must be rejected with a
/// diagnostic rather than reaching the unconditional
/// `panic!("type_decl() can't process Array Type.")`. A plain `List<T>` still
/// works.
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

/// Mutually-referential constants must terminate rather than recurse until the
/// stack overflows: the cycle guard has to survive a binary operator between
/// the two references. Non-cyclic chains that cross operators must resolve.
#[test]
fn cyclic_constants_do_not_overflow() {
    // Reaching the end of this call without aborting is the assertion.
    let _ = generate_ok("interface ICycle { const int A = B + 1; const int B = A + 1; }");

    assert!(
        generate_ok("interface IOk { const int A = 1; const int B = A + 1; const int C = A + B; }"),
        "non-cyclic constant chain must still resolve"
    );
}

/// An `i64::MAX` enumerator followed by an auto-increment member must wrap
/// (AOSP C++ semantics) rather than panic on a debug build's `enum_val += 1`.
#[test]
fn enum_autoincrement_overflow_wraps() {
    let src = "@Backing(type=\"long\") enum Big { MAXV = 9223372036854775807, NEXT }";
    let out = generate_str(src).expect("i64::MAX auto-increment must still generate");
    assert!(out.contains("r#MAXV = 9223372036854775807,"), "got: {out}");
    assert!(
        out.contains("r#NEXT = -9223372036854775808,"),
        "auto-increment past i64::MAX must wrap, got: {out}"
    );
}

/// An empty `{}` initializer must surface as a recoverable parse diagnostic in
/// all five parser positions where an aggregate initializer is not a valid
/// value (enumerator value, nested array element, annotation argument, named
/// annotation parameter, array dimension), not as an `unwrap()` panic.
/// Reaching the assertions without aborting is itself the regression guard. The legitimate empty-array initializer `int[] x = {}`
/// (the one position where `{}` is valid) must still parse + generate.
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

/// A negative byte literal inside an array default must be re-emitted
/// as its unsigned `u8` representation (AOSP `aidl_to_rust.cpp`). The array's
/// Rust element type is `u8` (i8 maps to u8 via `array_type_name`), which
/// cannot hold a negated literal, so the previous `[-1, ...]` / `vec![-1, ...]`
/// output did not compile. Positive bytes are unchanged.
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

/// A float/double field default that folds to a non-finite value
/// (e.g. `1.0e400` parses to infinity) must emit a valid Rust float constant
/// (`f64::INFINITY` / `f32::INFINITY` / `NAN`), not `inff64` / `NaNf32` which
/// do not compile. Finite defaults keep the suffixed-decimal form.
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

/// A non-nullable IBinder / ParcelFileDescriptor union member is stored as
/// `Option<T>` only for lack of `Default`. AOSP unwraps it with
/// UNEXPECTED_NULL on write and rejects an inbound null on read; it must not
/// silently cross the wire as a null marker in either direction. Mirrors the
/// `member.4` pattern already used on the parcelable write path. Assertions
/// are anchored per variant arm so an over-application to the `@nullable`
/// member cannot slip through.
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

/// Parcelable read side of the same contract: the write path already unwraps
/// a non-nullable IBinder field with UNEXPECTED_NULL, but the read path used
/// to accept an inbound null into the non-nullable field.
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

/// A fixed-size array dimension that fails to evaluate (or is non-positive)
/// must be a diagnostic, as in AOSP: folding it to 0 silently demotes the
/// field to a `Vec<T>`, a different wire format.
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

/// A fixed-size array default must supply exactly the declared element count,
/// and an array literal must not initialize a scalar target — either one emits
/// non-compiling Rust (`[i32; 2] = [1,2,3,]` / `i32 = &[]`) unless it is an
/// AIDL diagnostic.
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

/// Constants (and members) named `self`/`Self`/`super`/`crate` cannot be
/// emitted — `r#self` is not a valid Rust raw identifier — so they are
/// rejected at parse time instead of producing non-compiling output.
#[test]
fn reserved_path_keyword_member_names_are_diagnostics() {
    for src in [
        "interface IFoo { const int self = 1; }",
        "parcelable P { int crate; }",
    ] {
        assert!(!generate_ok(src), "reserved name must error: {src}");
    }
}

/// Unary operators on string literals (newly reachable now that `C_STR`
/// participates in the expression grammar) must be diagnostics — AOSP rejects
/// them; a silent pass-through would drop the operator.
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

/// A `const String[]` renders as `&[&str]` — its initializer elements are
/// emitted as string literals, which do not coerce to `&[String]` in const
/// position.
#[test]
fn const_string_array_renders_as_str_slice() {
    let out = generate_str("interface IFoo { const String[] S = {\"a\",\"b\"}; }")
        .expect("must generate");
    assert!(
        out.contains(r#"r#S: &[&str] = &["a","b",];"#),
        "const String[] must emit &[&str] (got: {out})"
    );
}

/// An enum discriminant referencing a sibling interface constant must fold to
/// the constant's value with correct auto-increment afterwards: a stale cache
/// entry from the pre-registration pass duplicates one wire discriminant
/// across members (`A = X, B` folding to A=5, B=5).
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

/// Float / char enum discriminants must be diagnostics, not lossy `to_i64`
/// truncations (`A = 1.5` silently becoming 1). Bool comparisons stay
/// legal — AOSP treats bool as integral in const expressions.
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

/// AOSP `ClassName` strips the leading `I` from an interface name only when
/// it is followed by an uppercase letter — stripping unconditionally garbles
/// `interface Foo3` into `Bnoo3`/`Bpoo3`.
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

/// Constant names must be emitted verbatim (AOSP Rust backend), not
/// upper-cased: `kMagicValue` stays `kMagicValue`, and `foo`/`FOO` remain
/// distinct constants instead of colliding (E0428).
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

/// A default whose type cannot convert to the declared type must be an AIDL
/// diagnostic (AOSP rejects it), not an unconverted emit
/// (`pub const r#A: i32 = "x";`) that only fails at the rustc stage.
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

/// String concatenation must compose through the ordinary expression grammar
/// (AOSP has a single expression grammar): a reference-first concat
/// (`A + "y"`) and a parenthesized concat (`("y" + "z")`) are both valid — a
/// string rule requiring a literal first operand would reject them.
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

/// An explicit empty (and non-empty) array constant must emit a slice
/// literal. Treating `const T[] X = {};` as "no initializer" would emit
/// `Default::default()`, which is not a const expression for `&[T]`
/// (E0658/E0015 at the rustc stage).
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

/// A `//` comment on the last line of a file without a trailing newline used
/// to be a parse error (LINE_COMMENT demanded `\n`).
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

// ---------------------------------------------------------------
// Codegen/API defects found in the 2026-08 review.
// ---------------------------------------------------------------

/// An unqualified constant reference must resolve inside its own declaration
/// (or an enclosing one), never against a same-named constant elsewhere in the
/// flat symbol table — that silently bakes a wrong wire value.
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

/// `@JavaOnlyImmutable` marks a *structured* parcelable that AOSP's Rust
/// backend generates normally; only `@JavaOnlyStableParcelable` means "no Rust
/// definition". A prefix match on `@JavaOnly` conflates them and emits a
/// fieldless struct with a live `Parcelable` impl — a silent wire break.
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

/// A declaration with no Rust representation must be a diagnostic, not a
/// fieldless struct whose `Parcelable` impl writes an empty payload.
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

/// An enum reference is its integral value in a binary expression (AOSP
/// `AidlConstantReference`). Ranking it above every arithmetic type made
/// comparisons asymmetric and truncated float operands.
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

/// AOSP's grammar accepts a single direction (`direction: IN | OUT | INOUT |
/// empty`). Accepting `direction*` kept only the last keyword, so a mistyped
/// `in out` silently generated `out` semantics.
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

/// `r#self` / `r#Self` / `r#super` / `r#crate` are not valid Rust raw
/// identifiers, and the bare forms are path keywords, so an AIDL name matching
/// one has no representation anywhere it is emitted.
#[test]
fn unrepresentable_identifiers_are_rejected_everywhere() {
    for src in [
        "package a; interface I { void self(); }",
        "package a; interface I { void f(in int crate); }",
        "package a; @Backing(type=\"int\") enum E { self = 1 }",
        "package a; interface self { void f(); }",
        "package a; parcelable crate { int x; }",
        "package a; parcelable P { int self; }",
    ] {
        assert!(!generate_ok(src), "must be rejected: {src}");
    }
}

/// A package segment may be a Rust keyword; the `pub mod` that declares it
/// must be escaped the same way `Namespace::relative_mod` escapes references
/// to it, or the two disagree.
#[test]
fn keyword_package_segment_is_escaped() {
    // The module nesting is built by `Builder::generate_all`, not by
    // `Generator::document`, so this has to go through the Builder.
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

/// An `out` argument whose type has no `Default` is stored as `Option<T>`
/// (AOSP `aidl_to_rust.cpp::RustNameOf`); its local is initialised with
/// `Default::default()`, which `SIBinder`/`ParcelFileDescriptor` lack. `inout`
/// reads its value from the parcel and stays unwrapped.
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

/// The trait signature and the server's local for an `inout` array must be the
/// same type — the server passes `&mut` its local straight into the call.
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

/// A fixed-size array constant is initialised with an array literal, which
/// does not coerce to a slice reference in const position.
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

/// An enum discriminant is emitted as a literal into a `[<backing>; N]`
/// newtype, where an out-of-range literal is a deny-by-default rustc error in
/// the generated crate. AOSP rejects it at AIDL-compile time.
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

/// `to_case(UpperCamel)` maps two distinct AIDL field names onto one Rust
/// variant; emitting both is `E0428`, so it must be a diagnostic.
#[test]
fn colliding_union_variant_names_are_a_diagnostic() {
    assert!(!generate_ok(
        "package a; union U { int my_field; int myField; }"
    ));
    assert!(generate_ok(
        "package a; union U { int my_field; int other; }"
    ));
}

/// The templates always emit `#[derive(Debug)]`; repeating it from
/// `@RustDerive` is a conflicting impl (`E0119`).
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

/// `rust_type` was the one declaration path that emitted the name unescaped.
#[test]
fn rust_type_declaration_name_is_escaped() {
    let out = generate_str("package a; parcelable type rust_type \"i32\";").expect("must generate");
    assert!(out.contains("pub mod r#type {"), "got:\n{out}");
    assert!(out.contains("pub type r#type = i32;"), "got:\n{out}");
}

/// `Path::is_dir()` follows symlinks, so a link cycle produced endlessly
/// deeper distinct path strings and the directory walk never terminated.
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
        Err(_) => panic!("the directory walk did not terminate on a symlink cycle"),
    }
}

/// The output directory is created on demand, whether `dest_dir` is missing
/// or `output` names a subdirectory.
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

/// Parsing happens in `generate()`, so the parser must be reset there: two
/// builders constructed before either generates must not share declarations.
/// `P` names `T` without importing it, so it resolves only if the first
/// builder's declaration table leaked into the second.
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

/// A fixed-size `String` constant is a by-value array of the `&str` literals
/// its initializer emits, like the `&[&str]` slice for the variable-length form.
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

/// An enum reference folds at its `@Backing` width, so an expression is not
/// widened just by naming a member of an `int`-backed enum.
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

/// `-E.A` and `~E.A` fold through the reference's integral value like `!E.A`
/// already does, rather than failing on the unfolded reference.
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

/// A package segment is emitted as a `mod` name, and `self`/`Self`/`super`/
/// `crate` cannot be raw identifiers, so the four are rejected there like in
/// every other name position. A dotted declaration name is checked per segment.
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

/// Every generated module carries the lint allowances as an inner attribute,
/// so a declaration is covered wherever it lands — including a document with
/// no `package`, which `generate_all` wraps in no module and therefore gives
/// no outer attribute to.
#[test]
fn every_generated_module_carries_the_lint_allowances() {
    let out =
        generate_str("parcelable A { int x; } parcelable B { int y; }").expect("must generate");
    assert_eq!(
        out.matches("#![allow(clippy::all, unused_imports,").count(),
        2,
        "got:\n{out}"
    );

    // The package-less path through `Builder`: no wrapping module, so the
    // inner attributes are the only ones there are.
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

/// A non-nullable `out` argument with no `Default` is stored as `Option<T>`;
/// leaving it unset must fail the transaction rather than write a null, and
/// an out `ParcelFileDescriptor` array is guarded at both lengths.
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

/// A `@nullable` array wraps its elements only for non-primitive, non-enum
/// element types (AOSP `UsesOptionInNullableVector`) — fixed-size arrays
/// included, where an `Option` element also has no `SerializeOption` impl.
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

/// A `@nullable` `String` array constant declares the element `Option` its
/// initializer emits, at both lengths.
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

/// The pre-parse guard counts a `<` as generic nesting only once a `>` closes
/// it, so a statement full of comparisons is not read as a deep generic.
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

/// An enum reference keeps its value when it does not fit the `@Backing`
/// type. `decl_enum`'s range check only runs when the enum itself is
/// generated, so narrowing here would silently emit a wrong constant.
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

/// `@RustDerive` follows AOSP's seven-trait schema: a name outside it either
/// duplicates an impl the templates always emit or names no trait at all.
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

/// An empty include path names the working directory — the spelling
/// `strip_package` derives for a source sitting directly under its own
/// package path. It must dedup against the other spellings of that directory
/// rather than listing it twice (every import under it then resolves to two
/// candidates) and rather than reaching cargo as a bare `rerun-if-changed=`.
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

/// Two spellings of one include directory (here a symlink and its target;
/// `./aidl` versus an absolute path is the same case) are one directory: an
/// import under it resolves to one file, not to an `AmbiguousImport`.
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

    // `include_dir` names the target; the source's package-derived include
    // directory is the link.
    rsbinder_aidl::Builder::new()
        .include_dir(root.join("aidl"))
        .source(root.join("link/hello/IHello.aidl"))
        .dest_dir(root.join("out"))
        .output("gen.rs")
        .generate()
        .expect("one directory under two names must not be an ambiguous import");
}

/// An empty hash is falsy to Tera and would silently emit no
/// `getInterfaceHash()` — the trap `Builder::version` already guards.
#[test]
#[should_panic(expected = "the hash must be non-empty")]
fn empty_interface_hash_is_rejected() {
    let dir = std::path::PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("empty_hash");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("I.aidl");
    std::fs::write(&src, "package a; interface I { void m(); }").unwrap();
    let _ = rsbinder_aidl::Builder::new().source(&src).hash("");
}

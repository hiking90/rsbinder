// SPDX-License-Identifier: Apache-2.0
//
// Regression tests for codegen defects found in the full-source review.
// Each case used to abort the AIDL compiler (hard `panic!` / stack
// overflow / debug overflow) on parseable input, violating the project's
// "no panic on user input" invariant. They must now surface as recoverable
// errors (or compute without panicking).

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

/// M-6: a `List<T[]>` (list-of-array) is grammar-valid but used to hit the
/// unconditional `panic!("type_decl() can't process Array Type.")`. It must
/// now be rejected with a diagnostic, while a plain `List<T>` still works.
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

/// M-7: mutually-referential constants used to recurse until the stack
/// overflowed (the cycle guard was abandoned when evaluation crossed a
/// binary operator). Generation must now terminate; non-cyclic chains that
/// also cross operators must still resolve.
#[test]
fn cyclic_constants_do_not_overflow() {
    // Reaching the end of this call without aborting is the assertion.
    let _ = generate_ok("interface ICycle { const int A = B + 1; const int B = A + 1; }");

    assert!(
        generate_ok("interface IOk { const int A = 1; const int B = A + 1; const int C = A + B; }"),
        "non-cyclic constant chain must still resolve"
    );
}

/// L-3: an `i64::MAX` enumerator followed by an auto-increment member used
/// to panic on debug builds (`enum_val += 1` overflow). It must now wrap
/// (AOSP C++ semantics) without aborting.
#[test]
fn enum_autoincrement_overflow_does_not_panic() {
    let src = "@Backing(type=\"long\") enum Big { MAXV = 9223372036854775807, NEXT }";
    // No panic / abort is the assertion.
    let _ = generate_ok(src);
}

/// An empty `{}` initializer used to `unwrap()`-panic in five parser
/// positions where an aggregate initializer is not a valid value
/// (enumerator value, nested array element, annotation argument, named
/// annotation parameter, array dimension). Each must now surface as a
/// recoverable parse diagnostic. Reaching the assertions without aborting
/// is itself the regression guard: a revert reintroduces the panic and
/// fails the test. The legitimate empty-array initializer `int[] x = {}`
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

/// AIDL-1: a negative byte literal inside an array default must be re-emitted
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

/// AIDL-2: a float/double field default that folds to a non-finite value
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
        packed.contains("1f64") || packed.contains("1.0f64") || packed.contains("1f64"),
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
/// used to fold to 0 via `unwrap_or(0)` and silently demote the field to a
/// `Vec<T>` — a different wire format. AOSP rejects it at build time.
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
/// and an array literal must not initialize a scalar target — both used to
/// emit non-compiling Rust (`[i32; 2] = [1,2,3,]` / `i32 = &[]`) instead of
/// an AIDL diagnostic.
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
/// the constant's value with correct auto-increment afterwards — a stale
/// cache entry from the pre-registration pass used to duplicate one wire
/// discriminant across members (`A = X, B` became A=5, B=5).
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
/// truncations (`A = 1.5` used to silently become 1). Bool comparisons stay
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
/// it is followed by an uppercase letter. `interface Foo3` used to generate
/// the garbled `Bnoo3`/`Bpoo3`.
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

/// A default whose type cannot convert to the declared type used to be
/// emitted unconverted (`pub const r#A: i32 = "x";`), only failing later at
/// the rustc stage. It must be an AIDL diagnostic (AOSP rejects it).
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
/// (`A + "y"`) and a parenthesized concat (`("y" + "z")`) used to be parse
/// errors because the legacy string rule required a literal first operand.
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

/// `const T[] X = {};` used to be silently treated as "no initializer" and
/// emitted `Default::default()` for a `&[T]` constant — not a const
/// expression (E0658/E0015 at the rustc stage). An explicit empty (and
/// non-empty) array constant must emit a slice literal.
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

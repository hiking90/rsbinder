// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Type-placement rules ported from AOSP `aidl`, and the `@deprecated`
//! javadoc tag.
//!
//! The `@FixedSize` and `@VintfStability` rules are contract-level: rsbinder
//! generates code that compiles either way, so without these checks an `.aidl`
//! authored against rsbinder could be refused by AOSP's `aidl` — the two
//! compilers must agree on what a valid contract is, not just on the wire
//! format. So are the argument, return-type and union-member forms of
//! `ParcelableHolder`. The `void` placements, the array/`List`/`@nullable`
//! forms of `ParcelableHolder`, and the field-less `union` are not
//! contract-level: they had no compiling Rust representation, so the check
//! turns a rustc error in the generated crate into an AIDL diagnostic.
//!
//! Each rule cites the AOSP source it mirrors.

use rsbinder_aidl::Builder;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

/// Every case goes through `Builder::generate`, not `parse_document` +
/// `Generator` directly, because only `Builder::generate` resets the parser's
/// thread-local declaration map. Driving the generator directly leaves each
/// case's types registered, so two cases in this file that reuse a type name
/// resolve against each other and the suite becomes order-dependent — which
/// `--test-threads=1` (one thread, one thread-local) would expose.
fn run(input: &str) -> Result<String, String> {
    static SEQ: AtomicU32 = AtomicU32::new(0);
    let id = SEQ.fetch_add(1, Ordering::Relaxed);

    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("placement_rules/{id}"));
    std::fs::create_dir_all(&dir).expect("create scratch dir");
    let aidl = dir.join("Case.aidl");
    std::fs::write(&aidl, input).expect("write aidl");

    let output = PathBuf::from("case.rs");
    match Builder::new()
        .source(aidl)
        .dest_dir(&dir)
        .output(output.clone())
        .generate()
    {
        Ok(()) => Ok(std::fs::read_to_string(dir.join(&output)).expect("read generated")),
        Err(e) => Err(format!("{e}")),
    }
}

/// Generate, expecting success, and return the rendered Rust.
fn generate(input: &str) -> String {
    run(input).unwrap_or_else(|e| panic!("expected generation to succeed, got {e}"))
}

/// Generate, expecting failure, and return the rendered message.
fn expect_error(input: &str) -> String {
    match run(input) {
        Err(e) => e,
        Ok(_) => panic!("expected an error, but generation succeeded"),
    }
}

fn assert_error_contains(input: &str, needle: &str) {
    let msg = expect_error(input);
    assert!(
        msg.contains(needle),
        "expected error containing {needle:?}, got {msg:?}"
    );
}

// ---------------------------------------------------------------------------
// `void` placement — AOSP `aidl_language.cpp`:
//   AidlTypeSpecifier::CheckValid       ("void type cannot be an array …")
//   AidlVariableDeclaration::CheckValid ("declarations cannot be of void type")
//   AidlMethod::CheckValid              ("'void' is an invalid type for the
//                                        parameter …")
// ---------------------------------------------------------------------------

#[test]
fn void_parameter_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void go(in void x);
}
        "#,
        "'void' is an invalid type for the parameter 'x'",
    );
}

#[test]
fn void_field_is_rejected() {
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    void v;
}
        "#,
        "declaration 'v' is void",
    );
}

#[test]
fn void_constant_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    const void V = 1;
    void go();
}
        "#,
        "declaration 'V' is void",
    );
}

#[test]
fn void_array_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void[] go();
}
        "#,
        "void type cannot be an array or nullable",
    );
}

#[test]
fn void_list_element_is_rejected() {
    // A `List<T>` shares the array representation, so the `void[]` check above
    // does not see this one. `Vec<()>` has no `DeserializeArray` counterpart:
    // without the check the generated crate fails in rustc, not here.
    assert_error_contains(
        r#"
package test;
interface IFoo {
    List<void> go();
}
        "#,
        "List element type cannot be void",
    );
}

#[test]
fn void_return_type_is_accepted() {
    let out = generate(
        r#"
package test;
interface IFoo {
    void go(in int x);
}
        "#,
    );
    assert!(out.contains("pub trait IFoo"), "{out}");
}

// ---------------------------------------------------------------------------
// `ParcelableHolder` placement — AOSP `aidl_language.cpp`:
//   AidlTypeSpecifier::CheckValid ("Arrays of ParcelableHolder are not
//                                  supported", "cannot be nullable")
//   AidlMethod::CheckValid        ("ParcelableHolder cannot be a return type")
//   AidlUnionDecl::CheckValid     ("A union can't have a member of
//                                  ParcelableHolder")
//   AidlArgument::CheckValid      ("ParcelableHolder cannot be an argument
//                                  type") — `aidl_typenames.cpp`
//                                  `GetArgumentAspect` gives the holder an
//                                  empty direction set.
// ---------------------------------------------------------------------------

#[test]
fn parcelable_holder_array_is_rejected() {
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    ParcelableHolder[] hs;
}
        "#,
        "arrays of ParcelableHolder are not supported",
    );
}

#[test]
fn parcelable_holder_list_element_is_rejected() {
    // As with `List<void>`: the array check does not cover the `List` form,
    // and `Vec<ParcelableHolder>` has no `SerializeArray` counterpart.
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    List<ParcelableHolder> hs;
}
        "#,
        "List element type cannot be ParcelableHolder",
    );
}

#[test]
fn nullable_parcelable_holder_is_rejected() {
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    @nullable ParcelableHolder h;
}
        "#,
        "ParcelableHolder cannot be nullable",
    );
}

#[test]
fn parcelable_holder_return_type_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    ParcelableHolder get();
}
        "#,
        "ParcelableHolder cannot be a return type",
    );
}

#[test]
fn parcelable_holder_union_member_is_rejected() {
    assert_error_contains(
        r#"
package test;
union Foo {
    int a = 0;
    ParcelableHolder h;
}
        "#,
        "cannot have a member of ParcelableHolder 'h'",
    );
}

#[test]
fn parcelable_holder_in_parameter_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void go(in ParcelableHolder h);
}
        "#,
        "ParcelableHolder cannot be an argument type ('h')",
    );
}

#[test]
fn parcelable_holder_out_parameter_is_rejected() {
    // AOSP refuses every direction, not just `in`: the aspect's direction set
    // is empty, so the check fires before the direction is ever consulted.
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void go(out ParcelableHolder h);
}
        "#,
        "ParcelableHolder cannot be an argument type ('h')",
    );
}

#[test]
fn parcelable_holder_field_is_accepted() {
    // The extension field of `ExtendableParcelable` — the whole point of the
    // type — must keep working.
    let out = generate(
        r#"
package test;
parcelable Foo {
    ParcelableHolder ext;
    int a;
}
        "#,
    );
    assert!(out.contains("ParcelableHolder"), "{out}");
}

// ---------------------------------------------------------------------------
// Field-less `union` — AOSP `aidl_language.cpp` `AidlUnionDecl::CheckValid`
// ("The union '…' has no fields.").
// ---------------------------------------------------------------------------

#[test]
fn union_without_members_is_rejected() {
    assert_error_contains(
        r#"
package test;
union Foo {
}
        "#,
        "the union 'Foo' has no fields",
    );
}

#[test]
fn union_with_only_constants_is_rejected() {
    // A `const` is not a field: the rendered enum would be uninhabited, so
    // `Default::default()` has nothing to return and the `write_to_parcel`
    // match — whose scrutinee is `&Self` — has no arm.
    assert_error_contains(
        r#"
package test;
union Foo {
    const int A = 1;
}
        "#,
        "the union 'Foo' has no fields",
    );
}

#[test]
fn union_with_one_field_and_a_constant_is_accepted() {
    let out = generate(
        r#"
package test;
union Foo {
    const int A = 1;
    int a = 0;
}
        "#,
    );
    assert!(out.contains("pub enum r#Foo"), "{out}");
}

// ---------------------------------------------------------------------------
// `@FixedSize` — AOSP `aidl_language.cpp` `AidlParcelable::CheckValid` +
// `aidl_typenames.cpp` `AidlTypenames::CanBeFixedSize`.
// ---------------------------------------------------------------------------

#[test]
fn fixed_size_rejects_string_field() {
    assert_error_contains(
        r#"
package test;
@FixedSize
parcelable Foo {
    int a = 0;
    String s;
}
        "#,
        "has a non-fixed size field named 's'",
    );
}

#[test]
fn fixed_size_rejects_variable_array() {
    assert_error_contains(
        r#"
package test;
@FixedSize
parcelable Foo {
    int[] a;
}
        "#,
        "has a non-fixed size field named 'a'",
    );
}

#[test]
fn fixed_size_rejects_nullable_field() {
    // `Inner` is fixed size on its own, so `@nullable` is the only violation
    // here — the rule under test is the one that fires.
    assert_error_contains(
        r#"
package test;
@FixedSize
parcelable Inner {
    int x;
}
@FixedSize
parcelable Foo {
    @nullable Inner i;
}
        "#,
        "has a non-fixed size field named 'i'",
    );
}

#[test]
fn fixed_size_rejects_non_fixed_parcelable_field() {
    assert_error_contains(
        r#"
package test;
parcelable Plain {
    int x;
}
@FixedSize
parcelable Foo {
    Plain p;
}
        "#,
        "has a non-fixed size field named 'p'",
    );
}

#[test]
fn fixed_size_union_is_checked_too() {
    assert_error_contains(
        r#"
package test;
@FixedSize
union Foo {
    int a = 0;
    String s;
}
        "#,
        "the @FixedSize union 'Foo' has a non-fixed size field named 's'",
    );
}

#[test]
fn fixed_size_accepts_primitives_enums_and_fixed_members() {
    // Mirrors the AOSP `FixedSize.aidl` fixture: an enum, a fixed-size array,
    // and a nested `@FixedSize` union are all fixed size.
    let out = generate(
        r#"
package test;
enum Kind { A = 1, B = 2 }
@FixedSize
union Inner {
    int a = 0;
    Kind k;
}
@FixedSize
parcelable Foo {
    boolean b;
    char c;
    double d;
    Kind k;
    Inner i;
    int[3] arr;
    int[2][3] matrix;
}
        "#,
    );
    assert!(out.contains("pub struct Foo"), "{out}");
}

#[test]
fn fixed_size_is_not_inherited_by_nested_types() {
    // AOSP reads `@FixedSize` with the plain `GetAnnotation`, not the scoped
    // lookup `@VintfStability` uses, so the nested parcelable is unconstrained.
    let out = generate(
        r#"
package test;
@FixedSize
parcelable Foo {
    parcelable Inner {
        String s;
    }
    int a;
}
        "#,
    );
    assert!(out.contains("pub struct Inner"), "{out}");
}

// ---------------------------------------------------------------------------
// `@VintfStability` — AOSP enforces this compilation-wide via
// `--stability vintf` (`aidl.cpp`); rsbinder enforces the reference closure
// that rule implies. `@VintfStability` is scoped (`GetScopedAnnotation`), so a
// nested declaration inherits it.
// ---------------------------------------------------------------------------

#[test]
fn vintf_interface_rejects_non_vintf_parameter() {
    assert_error_contains(
        r#"
package test;
parcelable Plain {
    int x;
}
@VintfStability
interface IFoo {
    void go(in Plain p);
}
        "#,
        "references 'test.Plain', which is not @VintfStability",
    );
}

#[test]
fn vintf_interface_rejects_non_vintf_return_type() {
    assert_error_contains(
        r#"
package test;
parcelable Plain {
    int x;
}
@VintfStability
interface IFoo {
    Plain go();
}
        "#,
        "references 'test.Plain', which is not @VintfStability",
    );
}

#[test]
fn vintf_parcelable_rejects_non_vintf_field() {
    assert_error_contains(
        r#"
package test;
parcelable Plain {
    int x;
}
@VintfStability
parcelable Foo {
    Plain f;
}
        "#,
        "references 'test.Plain', which is not @VintfStability",
    );
}

#[test]
fn vintf_rejects_non_vintf_array_element() {
    assert_error_contains(
        r#"
package test;
enum Kind { A = 1 }
@VintfStability
parcelable Foo {
    Kind[] ks;
}
        "#,
        "references 'test.Kind', which is not @VintfStability",
    );
}

#[test]
fn vintf_closure_of_vintf_types_is_accepted() {
    let out = generate(
        r#"
package test;
@VintfStability
parcelable Plain {
    int x;
}
@VintfStability
interface IFoo {
    Plain go(in Plain[] p);
}
        "#,
    );
    assert!(out.contains("pub trait IFoo"), "{out}");
}

#[test]
fn non_vintf_type_may_reference_a_vintf_type() {
    // Only the VINTF direction is constrained.
    let out = generate(
        r#"
package test;
@VintfStability
parcelable V {
    int x;
}
interface IFoo {
    void go(in V v);
}
        "#,
    );
    assert!(out.contains("pub trait IFoo"), "{out}");
}

#[test]
fn builtin_types_are_not_part_of_the_vintf_closure() {
    // `ParcelableHolder` is what a VINTF extendable parcelable exists for; it
    // is a builtin, not a user-defined type, so it never trips the closure.
    let out = generate(
        r#"
package test;
@VintfStability
parcelable Foo {
    ParcelableHolder ext;
    int a;
    String s;
    IBinder b;
}
        "#,
    );
    assert!(out.contains("pub struct Foo"), "{out}");
}

#[test]
fn nested_type_inherits_vintf_stability() {
    // AOSP `GetScopedAnnotation` walks to the enclosing type, so `Inner` is
    // VINTF-stable without repeating the annotation — and must report that
    // stability, which is what a `ParcelableHolder` records for it.
    let out = generate(
        r#"
package test;
@VintfStability
parcelable Foo {
    parcelable Inner {
        int x;
        ParcelableHolder ext;
    }
    Inner i;
}
        "#,
    );
    assert_eq!(
        out.matches("fn stability(&self) -> rsbinder::Stability { rsbinder::Stability::Vintf }")
            .count(),
        2,
        "both Foo and its nested Inner must report Vintf stability:\n{out}"
    );
    // The wire-visible half: the holder inside `Inner` records the inherited
    // stability as its own byte.
    assert_eq!(
        out.matches("ParcelableHolder::new(rsbinder::Stability::Vintf)")
            .count(),
        1,
        "nested Inner's holder must inherit Vintf stability (wire byte 1):\n{out}"
    );
}

// ---------------------------------------------------------------------------
// `@deprecated` javadoc — AOSP `comments.cpp` `FindDeprecated` +
// `generate_rust.cpp` `GenerateDeprecated`.
// ---------------------------------------------------------------------------

#[test]
fn deprecated_interface_and_method() {
    let out = generate(
        r#"
package test;
/** @deprecated use IBar */
interface IFoo {
    /** @deprecated gone in v2 */
    void old();
    void current();
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "use IBar"]"#), "{out}");
    assert!(out.contains(r#"#[deprecated = "gone in v2"]"#), "{out}");
    // Exactly one method carries it.
    assert_eq!(out.matches("gone in v2").count(), 1, "{out}");
}

#[test]
fn deprecated_without_a_note_renders_bare() {
    let out = generate(
        r#"
package test;
/** @deprecated */
parcelable Foo {
    int a;
}
        "#,
    );
    assert!(out.contains("#[deprecated]\n"), "{out}");
    assert!(!out.contains("#[deprecated ="), "{out}");
}

#[test]
fn deprecated_note_spanning_lines_is_joined() {
    let out = generate(
        r#"
package test;
/**
 * @deprecated first line
 *     second line
 */
interface IFoo {
    void go();
}
        "#,
    );
    assert!(
        out.contains(r#"#[deprecated = "first line second line"]"#),
        "{out}"
    );
}

#[test]
fn deprecated_applies_to_fields_constants_and_enumerators() {
    let out = generate(
        r#"
package test;
enum Kind {
    A = 1,
    /** @deprecated bad value */
    B = 2,
}
parcelable Foo {
    /** @deprecated bad field */
    int a;
    int b;
}
interface IFoo {
    /** @deprecated bad const */
    const int C = 1;
    void go();
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "bad value"]"#), "{out}");
    assert!(out.contains(r#"#[deprecated = "bad field"]"#), "{out}");
    assert!(out.contains(r#"#[deprecated = "bad const"]"#), "{out}");
}

#[test]
fn deprecated_applies_to_union_and_its_members() {
    let out = generate(
        r#"
package test;
/** @deprecated old union */
union Foo {
    int a = 0;
    /** @deprecated old member */
    String s;
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "old union"]"#), "{out}");
    assert!(out.contains(r#"#[deprecated = "old member"]"#), "{out}");
}

#[test]
fn deprecated_survives_intervening_annotations() {
    // The javadoc precedes the annotations, so the item's span start (which
    // includes them) must still find it.
    let out = generate(
        r#"
package test;
/** @deprecated annotated */
@VintfStability
parcelable Foo {
    int a;
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "annotated"]"#), "{out}");
}

#[test]
fn a_trailing_line_comment_detaches_the_javadoc() {
    // AOSP `GetValidComment`: only the *last* comment of the run counts, and
    // only when it is a block comment.
    let out = generate(
        r#"
package test;
/** @deprecated ignored */
// a line comment after the javadoc
interface IFoo {
    void go();
}
        "#,
    );
    assert!(!out.contains("#[deprecated"), "{out}");
}

#[test]
fn a_block_comment_without_tags_is_not_deprecation() {
    let out = generate(
        r#"
package test;
/**
 * Just documentation, no tags at all.
 */
interface IFoo {
    void go();
}
        "#,
    );
    assert!(!out.contains("#[deprecated"), "{out}");
}

#[test]
fn a_deprecated_looking_string_constant_is_not_a_comment() {
    // The comment scanner must skip string literals: a `/*` inside one is
    // data, not a comment opener. Were it treated as an opener, it would
    // swallow the real javadoc below it up to the first `*/` and the tag
    // would be lost.
    let out = generate(
        r#"
package test;
interface IFoo {
    const String S = "/* oops";
    /** @deprecated real */
    void go();
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "real"]"#), "{out}");
}

#[test]
fn a_note_with_quotes_is_escaped() {
    let out = generate(
        r#"
package test;
/** @deprecated use "IBar" instead */
interface IFoo {
    void go();
}
        "#,
    );
    assert!(
        out.contains(r#"#[deprecated = "use \"IBar\" instead"]"#),
        "{out}"
    );
}

#[test]
fn generated_modules_allow_deprecated_internally() {
    // The generated proxy/dispatch plumbing names the deprecated method, so
    // the module-scoped allow has to be there or the output cannot build
    // under `-D warnings`. It stops at the module boundary, so a consumer
    // still sees the deprecation.
    let out = generate(
        r#"
package test;
interface IFoo {
    /** @deprecated gone */
    void old();
}
        "#,
    );
    assert!(out.contains(r#"#[deprecated = "gone"]"#), "{out}");
    assert!(out.contains("deprecated)]"), "{out}");
}

// ---------------------------------------------------------------------------
// Rules AOSP enforces that rsbinder had not ported at all. Each of these was a
// silent divergence rather than a gap in the checks above.
// ---------------------------------------------------------------------------

#[test]
fn vintf_interface_declares_vintf_stability() {
    // AOSP `generate_rust.cpp` emits `stability: …::Stability::Vintf` into
    // `declare_binder_interface!` for a `@VintfStability` interface. Without
    // it the binder registers with the default `System` stability, and a peer
    // that requires VINTF refuses it — a wire-visible difference, not a
    // cosmetic one.
    let out = generate(
        r#"
package test;
@VintfStability
interface IFoo {
    void go();
}
        "#,
    );
    assert!(
        out.contains("stability: rsbinder::Stability::Vintf"),
        "{out}"
    );
}

#[test]
fn non_vintf_interface_declares_no_stability() {
    let out = generate(
        r#"
package test;
interface IFoo {
    void go();
}
        "#,
    );
    assert!(!out.contains("Stability::Vintf"), "{out}");
}

#[test]
fn a_nested_type_resolves_against_a_grandparent_scope() {
    // AOSP `AidlDefinedType::ResolveName` recurses through every enclosing
    // scope. `C` is three levels deep and names `D`, which lives beside its
    // grandparent `B` — legal AIDL that a two-level walk rejects as an
    // unknown type.
    let out = generate(
        r#"
package test;
parcelable A {
    parcelable D {
        int x;
    }
    parcelable B {
        parcelable C {
            D field;
        }
    }
}
        "#,
    );
    assert!(out.contains("pub r#field: super::super::D::D"), "{out}");
}

#[test]
fn duplicate_method_name_is_rejected() {
    // The second declaration would otherwise collapse into the first and take
    // its transaction code with it, silently renumbering everything after it.
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void m();
    void m(int a);
}
        "#,
        "interface 'IFoo' has a duplicate method name 'm'",
    );
}

#[test]
fn duplicate_argument_name_is_rejected() {
    // Both arguments render as the same `_arg_a` binding, so leaving this to
    // the consumer's build surfaces it as rustc E0415 instead of an AIDL
    // diagnostic. AOSP `AidlMethod::CheckValid` keeps an `argument_names` set.
    assert_error_contains(
        r#"
package test;
interface IFoo {
    void m(int a, int a);
}
        "#,
        "method 'm' has a duplicate argument name 'a'",
    );
}

#[test]
fn a_generic_on_a_non_generic_type_is_rejected() {
    // AOSP `AidlTypeSpecifier::CheckValid`: only `List`, `Map`, and a
    // parameterizable user-defined type take type arguments. The generic was
    // being dropped, so `String<int>` silently became a plain `String`.
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    String<int> s;
}
        "#,
        "'String' is not a generic type",
    );
}

#[test]
fn a_generic_user_defined_type_still_resolves() {
    // The new gate must not touch a parameterizable parcelable — AOSP's
    // `generic/Pair.aidl` fixture shape.
    let out = generate(
        r#"
package test;
parcelable Pair<A, B> {
    int x;
}
parcelable Uses {
    Pair<int, String> p;
}
        "#,
    );
    assert!(out.contains("pub struct Uses"), "{out}");
}

#[test]
fn a_user_defined_constant_type_is_rejected() {
    // AOSP `AidlConstantDeclaration::CheckValid` admits only `{String, byte,
    // int, long, float, double}`. rsbinder keeps its `boolean`/`char`/array
    // extensions but enforces the part that is not one — which is also what
    // keeps a non-VINTF type out of a `@VintfStability` declaration through a
    // constant, the one reference position the closure walk does not cover.
    assert_error_contains(
        r#"
package test;
enum Kind { A = 1 }
@VintfStability
interface IFoo {
    const Kind C = Kind.A;
    void go();
}
        "#,
        "constant 'C' has an unsupported type",
    );
}

#[test]
fn a_binder_constant_type_is_rejected() {
    assert_error_contains(
        r#"
package test;
interface IFoo {
    const IBinder C = 1;
    void go();
}
        "#,
        "constant 'C' has an unsupported type",
    );
}

#[test]
fn primitive_string_and_array_constants_still_work() {
    // rsbinder deliberately allows more constant types than AOSP — `boolean`,
    // `char`, and constant arrays. The narrower check must not take those away.
    let out = generate(
        r#"
package test;
interface IFoo {
    const boolean B = true;
    const char C = 'z';
    const int I = 1;
    const long L = 2;
    const byte BY = 3;
    const float F = 1.0f;
    const double D = 2.0;
    const String S = "s";
    const int[] ARR = {1, 2, 3};
    void go();
}
        "#,
    );
    assert!(out.contains("pub trait IFoo"), "{out}");
}

#[test]
fn a_stable_api_parcelable_is_exempt_from_the_vintf_closure() {
    // AOSP exempts a stable-API parcelable from the `--stability vintf` sweep
    // (`IsStableApiParcelable`); for Rust that is `@RustOnlyStableParcelable`,
    // whose type comes from `rust_type` and so has no declaration to annotate.
    let out = generate(
        r#"
package test;
@RustOnlyStableParcelable
parcelable Dur rust_type "std::time::Duration";
@VintfStability
interface IFoo {
    void go(in Dur d);
}
        "#,
    );
    assert!(out.contains("pub trait IFoo"), "{out}");
}

#[test]
fn map_keeps_its_own_unsupported_diagnostic() {
    // `Map` is generic, but it reaches the unknown-type path, and that message
    // is the one the AOSP fixture sweep pins. The generic gate must not shadow
    // it with "not a generic type", which would be actively misleading.
    assert_error_contains(
        r#"
package test;
parcelable Foo {
    Map<String, int> m;
}
        "#,
        "unknown type 'Map'",
    );
}

// SPDX-License-Identifier: Apache-2.0
//
// Recognition-only annotation lock-ins.
//
// AOSP's Rust AIDL backend (`system/tools/aidl/generate_rust.cpp`)
// silently ignores three annotations whose effects are either
// constraint-only, automatically true in rsbinder, or Java-backend
// specific. rsbinder-aidl matches the Rust backend's silent-ignore
// behavior. These tests lock that in:
//
//   * `@FixedSize` (parcelable / union / type-param) — AOSP linter
//     constraint; the wire format is the same as without.
//   * `@SensitiveData` (interface) — AOSP `generate_cpp.cpp:246` /
//     `generate_rust.cpp:365` add `FLAG_CLEAR_BUF` to every method on
//     the interface. rsbinder unconditionally emits `FLAG_CLEAR_BUF`
//     for **every** generated method (see
//     `rsbinder-aidl/src/generator.rs` `submit_transact` lines), so
//     the on-the-wire effect AOSP wants is universal in our output.
//   * `@PropagateAllowBlocking` (method) — emitted *only* by AOSP's
//     Java backend (`generate_java_binder.cpp:816`); the C++ and Rust
//     backends do not emit anything for it. rsbinder follows the Rust
//     backend.

fn generate(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).expect("generate").1
}

fn warnings_for(input: &str) -> Vec<String> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let doc = rsbinder_aidl::parse_document(&ctx).expect("parse");
    doc.warnings.iter().map(|w| w.message.clone()).collect()
}

// ---------------------------------------------------------------
// @FixedSize — AOSP `aidl_language.cpp:189-192` — applies to
// structured parcelable / union / type parameter; no codegen effect.
// ---------------------------------------------------------------

#[test]
fn fixed_size_parcelable_byte_identical() {
    let plain = generate(
        r#"
package test;
parcelable Foo {
    int x;
    int y;
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
@FixedSize
parcelable Foo {
    int x;
    int y;
}
        "#,
    );
    assert_eq!(
        plain, annotated,
        "@FixedSize must not change parcelable codegen"
    );
}

#[test]
fn fixed_size_union_byte_identical() {
    let plain = generate(
        r#"
package test;
union FooUnion {
    int x;
    long y;
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
@FixedSize
union FooUnion {
    int x;
    long y;
}
        "#,
    );
    assert_eq!(plain, annotated, "@FixedSize must not change union codegen");
}

#[test]
fn fixed_size_is_recognized_no_warning() {
    let warnings = warnings_for(
        r#"
package test;
@FixedSize
parcelable Foo {
    int x;
}
        "#,
    );
    let fixed_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.contains("@FixedSize"))
        .collect();
    assert!(
        fixed_warnings.is_empty(),
        "@FixedSize must be recognized: {fixed_warnings:?}"
    );
}

// ---------------------------------------------------------------
// @SensitiveData — AOSP `aidl_language.cpp:140`:
// `CONTEXT_TYPE_INTERFACE`, no parameters. AOSP `generate_cpp.cpp:246`
// and `generate_rust.cpp:365` add `FLAG_CLEAR_BUF` to every method.
// rsbinder unconditionally emits `FLAG_CLEAR_BUF` for every generated
// method (see `generator.rs` `submit_transact` calls), so the wire
// effect is identical with or without the annotation.
// ---------------------------------------------------------------

#[test]
fn sensitive_data_interface_byte_identical() {
    let plain = generate(
        r#"
package test;
interface IFoo {
    String getSecret();
    void setSecret(in String secret);
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
@SensitiveData
interface IFoo {
    String getSecret();
    void setSecret(in String secret);
}
        "#,
    );
    assert_eq!(
        plain, annotated,
        "@SensitiveData must not change codegen — rsbinder already \
         emits FLAG_CLEAR_BUF for every method"
    );
}

#[test]
fn sensitive_data_already_universal_clear_buf() {
    // Defensive regression: confirm rsbinder's *un-annotated* interface
    // still emits `FLAG_CLEAR_BUF` for every transaction. If this ever
    // becomes opt-in (so the un-annotated baseline drops the flag), the
    // `@SensitiveData` lock-in above silently regresses and we need a
    // real codegen branch.
    let out = generate(
        r#"
package test;
interface IFoo {
    String getSecret();
    void setSecret(in String secret);
}
        "#,
    );
    let clear_buf_count = out.matches("FLAG_CLEAR_BUF").count();
    assert!(
        clear_buf_count >= 2,
        "rsbinder must keep emitting FLAG_CLEAR_BUF for every method \
         (else @SensitiveData lock-in regresses). count={clear_buf_count}\n{out}"
    );
}

#[test]
fn sensitive_data_is_recognized_no_warning() {
    let warnings = warnings_for(
        r#"
package test;
@SensitiveData
interface IFoo {
    void run();
}
        "#,
    );
    let sd_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.contains("@SensitiveData"))
        .collect();
    assert!(
        sd_warnings.is_empty(),
        "@SensitiveData must be recognized: {sd_warnings:?}"
    );
}

// ---------------------------------------------------------------
// @PropagateAllowBlocking — AOSP emits codegen for this *only* in the
// Java backend (`generate_java_binder.cpp:816` calls
// `_reply.setPropagateAllowBlocking()`); both `generate_cpp.cpp` and
// `generate_rust.cpp` silently ignore it. rsbinder matches the Rust
// backend.
// ---------------------------------------------------------------

#[test]
fn propagate_allow_blocking_byte_identical() {
    let plain = generate(
        r#"
package test;
interface IFoo {
    IBinder getBinder();
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
interface IFoo {
    @PropagateAllowBlocking IBinder getBinder();
}
        "#,
    );
    assert_eq!(
        plain, annotated,
        "@PropagateAllowBlocking must not change Rust codegen (Java-only \
         in AOSP, see generate_java_binder.cpp:816)"
    );
}

#[test]
fn propagate_allow_blocking_is_recognized_no_warning() {
    let warnings = warnings_for(
        r#"
package test;
interface IFoo {
    @PropagateAllowBlocking IBinder getBinder();
}
        "#,
    );
    let pab_warnings: Vec<_> = warnings
        .iter()
        .filter(|w| w.contains("@PropagateAllowBlocking"))
        .collect();
    assert!(
        pab_warnings.is_empty(),
        "@PropagateAllowBlocking must be recognized: {pab_warnings:?}"
    );
}

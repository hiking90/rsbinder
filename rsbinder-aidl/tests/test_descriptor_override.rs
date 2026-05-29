// SPDX-License-Identifier: Apache-2.0
//
// `@Descriptor("X")` interface-descriptor override.
//
// The annotation is *already* parsed and consumed by the generator
// (see `parser::get_descriptor_from_annotation_list` callers in
// `generator.rs`). These tests are a lock-in: they fail if a future
// refactor breaks the override for any of the three top-level
// declarations (interface / parcelable / union) that AOSP allows it on.
//
// AOSP reference: `aidl_language.cpp:179` registers `@Descriptor` under
// `CONTEXT_DECL | CONTEXT_TYPE_INTERFACE | CONTEXT_TYPE_PARCELABLE |
// CONTEXT_TYPE_UNION` with a single required `value = kStringType`.

fn generate(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).expect("generate").1
}

#[test]
fn descriptor_override_replaces_interface_namespace() {
    let out = generate(
        r#"
package test.pkg;

@Descriptor(value = "android.os.IFoo")
interface IFoo {
    void run();
}
        "#,
    );
    // The wire-shipped descriptor must be the override, not the
    // package-derived `test.pkg.IFoo`.
    assert!(
        out.contains("\"android.os.IFoo\""),
        "override descriptor missing in generated output:\n{out}"
    );
    assert!(
        !out.contains("\"test.pkg.IFoo\""),
        "package-derived descriptor leaked despite @Descriptor override:\n{out}"
    );
}

#[test]
fn descriptor_override_replaces_parcelable_namespace() {
    let out = generate(
        r#"
package test.pkg;

@Descriptor(value = "android.os.Foo")
parcelable Foo {
    int x;
}
        "#,
    );
    assert!(
        out.contains("\"android.os.Foo\""),
        "override descriptor missing in generated parcelable:\n{out}"
    );
    assert!(
        !out.contains("\"test.pkg.Foo\""),
        "package-derived descriptor leaked despite @Descriptor override:\n{out}"
    );
}

#[test]
fn descriptor_override_replaces_union_namespace() {
    let out = generate(
        r#"
package test.pkg;

@Descriptor(value = "android.os.FooUnion")
union FooUnion {
    int x;
    String y;
}
        "#,
    );
    assert!(
        out.contains("\"android.os.FooUnion\""),
        "override descriptor missing in generated union:\n{out}"
    );
    assert!(
        !out.contains("\"test.pkg.FooUnion\""),
        "package-derived descriptor leaked despite @Descriptor override:\n{out}"
    );
}

#[test]
fn no_descriptor_annotation_uses_package_path() {
    // R2 regression: an interface without `@Descriptor` must still wire
    // up the package-derived descriptor (`test.pkg.IFoo`).
    let out = generate(
        r#"
package test.pkg;

interface IFoo {
    void run();
}
        "#,
    );
    assert!(
        out.contains("\"test.pkg.IFoo\""),
        "package-derived descriptor missing on un-annotated interface:\n{out}"
    );
}

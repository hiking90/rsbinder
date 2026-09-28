// SPDX-License-Identifier: Apache-2.0

//! `@Descriptor("X")` interface-descriptor override.
//!
//! The annotation is parsed and consumed by the generator
//! (see `parser::get_descriptor_from_annotation_list` callers in
//! `generator.rs`). These tests are a lock-in: they fail if a
//! refactor breaks the override for any of the three top-level
//! declarations (interface / parcelable / union) that rsbinder accepts it on.
//!
//! AOSP reference: `aidl_language.cpp` `AllSchemas()` registers `@Descriptor`
//! for interfaces only (`CONTEXT_TYPE_INTERFACE`), with a single required
//! `value = kStringType`. rsbinder also honours it on parcelables and unions,
//! an input AOSP's `aidl` rejects.

fn generate(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).expect("generate").1
}

/// AOSP grammar: `@Descriptor("X")` is `@Descriptor(value = "X")`.
#[test]
fn positional_descriptor_override_replaces_interface_namespace() {
    let out = generate(
        r#"
package test.pkg;

@Descriptor("android.os.IFoo")
interface IFoo {
    void run();
}
        "#,
    );
    assert!(out.contains("\"android.os.IFoo\""), "{out}");
    assert!(!out.contains("\"test.pkg.IFoo\""), "{out}");
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
    // The wire-shipped descriptor must be the override, not the package-derived `test.pkg.IFoo`.
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

/// AOSP `ParamValue<std::string>` evaluates the value, so a concatenation is folded.
#[test]
fn descriptor_value_is_a_folded_constant_expression() {
    let out = generate(
        r#"
package test.pkg;

@Descriptor(value = "a.b." + "IOld")
interface IFoo {
    void run();
}
        "#,
    );
    assert!(out.contains("\"a.b.IOld\""), "{out}");
    assert!(!out.contains("a.b. + IOld"), "{out}");
}

/// AOSP `AidlAnnotation::CheckValid()` refuses each of these instead of using the package path.
#[test]
fn ill_formed_descriptor_is_rejected() {
    for (args, expected) in [
        ("value = 5", "Invalid value for parameter value"),
        ("valu = \"x\"", "Parameter valu not supported"),
        ("", "Missing 'value'"),
        ("value = test.pkg.X", "contains reference to test.pkg.X"),
    ] {
        let input =
            format!("package test.pkg; @Descriptor({args}) interface IFoo {{ void run(); }}");
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", &input);
        let err = match rsbinder_aidl::parse_document(&ctx) {
            Err(e) => e,
            Ok(document) => rsbinder_aidl::Generator::new(false, false)
                .document(&document)
                .expect_err("ill-formed @Descriptor must be rejected"),
        };
        assert!(format!("{err:?}").contains(expected), "{args}: {err:?}");
    }
}

#[test]
fn no_descriptor_annotation_uses_package_path() {
    // Without `@Descriptor` the package-derived descriptor (`test.pkg.IFoo`) is used.
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

/// AOSP `AidlInterface::GetDescriptor`: an empty `@Descriptor("")` is no override.
#[test]
fn empty_descriptor_falls_back_to_package_path() {
    let out = generate("package test.pkg; @Descriptor(\"\") interface IFoo { void run(); }");
    assert!(out.contains("\"test.pkg.IFoo\""), "{out}");
    assert!(!out.contains("\"\""), "empty descriptor emitted:\n{out}");
}

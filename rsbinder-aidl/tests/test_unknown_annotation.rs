// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AOSP `AidlAnnotation::AllSchemas()` defines a
//! closed set of 23 recognised annotations. Anything else used to be
//! silently dropped — so a typo like `@RustDrive` would compile clean
//! with no codegen effect and no diagnostic. The parser now records
//! such cases as `Document::warnings`, which `Builder::generate()`
//! relays to cargo as `cargo:warning=...` lines.

use rsbinder_aidl::{parse_document, SourceContext};

fn warnings_for(input: &str) -> Vec<String> {
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx).expect("parse should succeed");
    doc.warnings.iter().map(|w| w.message.clone()).collect()
}

#[test]
fn typo_annotation_produces_warning() {
    let warnings = warnings_for(
        r#"
@RustDrive
parcelable Foo {
    int x;
}
"#,
    );
    assert!(
        warnings.iter().any(|m| m.contains("@RustDrive")),
        "no warning emitted for `@RustDrive` typo: {warnings:?}"
    );
}

#[test]
fn known_annotations_produce_no_warning() {
    // A spread of name-handled, silent-allowlisted, and Java-only
    // annotations — all of which appear in AOSP's `AllSchemas()`.
    let warnings = warnings_for(
        r#"
@VintfStability
@JavaDerive(equals=true)
@SuppressWarnings(value={"inout-parameter"})
interface IFoo {
    void run(@nullable @utf8InCpp String x);
}
"#,
    );
    assert!(
        warnings.is_empty(),
        "unexpected warnings for known annotations: {warnings:?}"
    );
}

#[test]
fn warnings_do_not_block_parsing() {
    // The compilation should still produce a usable Document even when
    // the AIDL carries an unrecognised annotation.
    let ctx = SourceContext::new(
        "test.aidl",
        r#"
@TotallyMadeUp
parcelable Foo {
    int x;
}
"#,
    );
    let doc = parse_document(&ctx).expect("parse should succeed");
    assert_eq!(doc.warnings.len(), 1, "expected exactly one warning");
    assert_eq!(doc.decls.len(), 1, "Foo parcelable should still parse");
}

#[test]
fn warnings_from_previous_parse_do_not_leak() {
    // Make sure CURRENT_WARNINGS is properly scoped to each parse call.
    let warn1 = warnings_for(
        r#"
@TotallyMadeUp
parcelable Foo { int x; }
"#,
    );
    assert!(!warn1.is_empty());

    let warn2 = warnings_for(
        r#"
parcelable Bar { int y; }
"#,
    );
    assert!(
        warn2.is_empty(),
        "warnings leaked from previous parse: {warn2:?}"
    );
}

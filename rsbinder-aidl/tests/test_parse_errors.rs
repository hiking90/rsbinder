// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Phase 2 parse error tests (task 5.2)
//! Validates that AIDL syntax errors return Err (not panic) with proper diagnostics.

use miette::Diagnostic;
use rsbinder_aidl::{parse_document, AidlError, SourceContext};

/// Helper: parse AIDL input and expect a parse error
fn expect_parse_error(input: &str, filename: &str) -> AidlError {
    let ctx = SourceContext::new(filename, input);
    match parse_document(&ctx) {
        Err(e) => e,
        Ok(_) => panic!("Expected parse error but parsing succeeded"),
    }
}

// 2.1a: Missing semicolon
#[test]
fn test_missing_semicolon() {
    let err = expect_parse_error("parcelable Foo {\n    int field\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
    if let AidlError::Parse(pe) = &err {
        assert_eq!(pe.code().unwrap().to_string(), "aidl::parse_error");
    }
}

// 2.1b: Completely invalid input
#[test]
fn test_completely_invalid_input() {
    let err = expect_parse_error("this is not valid aidl at all", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
    if let AidlError::Parse(pe) = &err {
        assert_eq!(pe.code().unwrap().to_string(), "aidl::parse_error");
    }
}

// 2.1c: Empty document (no declarations, only package)
#[test]
fn test_empty_document() {
    let err = expect_parse_error("package android.test;", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1d: Keyword used as identifier
#[test]
fn test_keyword_as_identifier() {
    let err = expect_parse_error("parcelable interface {\n    int val;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1e: Unclosed brace
#[test]
fn test_unclosed_brace() {
    let err = expect_parse_error("parcelable Foo {\n    int val;\n", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1f: Invalid type syntax (digits before type name)
#[test]
fn test_invalid_type_syntax() {
    let err = expect_parse_error("parcelable Foo {\n    123int val;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1g: Duplicate package declaration
#[test]
fn test_duplicate_package() {
    let err = expect_parse_error("package a;\npackage b;\nparcelable Foo {}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1h: Missing method parentheses
#[test]
fn test_missing_method_parens() {
    let err = expect_parse_error("interface IFoo {\n    void method;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1i: Error includes filename in source context
#[test]
fn test_error_includes_filename() {
    let err = expect_parse_error("this is invalid", "hello.aidl");
    if let AidlError::Parse(pe) = &err {
        // The NamedSource should have the filename we provided
        let source_code = pe.source_code().expect("must have source_code");
        // Read the first byte to verify source is attached
        let content = source_code
            .read_span(&miette::SourceSpan::new(0.into(), 0), 0, 0)
            .expect("must be readable");
        assert!(
            content.name().unwrap_or("").contains("hello.aidl"),
            "Expected filename 'hello.aidl' in source name, got: {:?}",
            content.name()
        );
    } else {
        panic!("Expected AidlError::Parse, got: {err:?}");
    }
}

// 2.1j: Error span points to correct location
#[test]
fn test_error_span_points_to_correct_location() {
    let input = "parcelable Foo {\n    int 123bad;\n}";
    let err = expect_parse_error(input, "test.aidl");
    if let AidlError::Parse(pe) = &err {
        let labels: Vec<_> = pe.labels().expect("must have labels").collect();
        assert!(!labels.is_empty(), "must have at least one label");
        // The span should point somewhere in the input (not out of bounds)
        let offset = labels[0].inner().offset();
        assert!(
            offset <= input.len(),
            "span offset {offset} exceeds input length {}",
            input.len()
        );
    } else {
        panic!("Expected AidlError::Parse, got: {err:?}");
    }
}

// AIDL-3 / AIDL-5: a backslash or control byte in a String constant would be
// emitted verbatim into the generated Rust `"..."` and fail to compile (a raw
// `\X` is not necessarily a valid Rust escape; rsbinder does not decode string
// escapes). They are rejected at parse time. Non-ASCII (UTF-8) text stays
// valid — rsbinder is intentionally more lenient than AOSP there.
#[test]
fn test_string_constant_backslash_or_control_rejected() {
    for src in [
        r#"parcelable P { const String S = "a\nb"; }"#, // backslash-n (not decoded)
        r#"parcelable P { const String S = "a\\b"; }"#, // literal backslash
        r#"parcelable P { const String S = "a\"b"; }"#, // escaped quote (AIDL-5 grammar)
    ] {
        let err = expect_parse_error(src, "test.aidl");
        assert!(matches!(&err, AidlError::Parse(_)), "src: {src}");
    }
    // Non-ASCII text and plain ASCII stay valid.
    for src in [
        r#"parcelable P { const String S = "한글 테스트"; }"#,
        r#"parcelable P { const String S = "plain ascii"; }"#,
    ] {
        let ctx = SourceContext::new("test.aidl", src);
        assert!(
            parse_document(&ctx).is_ok(),
            "string constant must still parse: {src}"
        );
    }
}

// AIDL-4: an unsupported char escape used to fall through to the post-backslash
// char verbatim, silently producing the wrong code point (`'\a'` -> 'a' = 97,
// not bell = 7). It is now rejected. The supported escapes still decode and a
// plain char still parses.
#[test]
fn test_char_constant_unknown_escape_rejected() {
    for src in [
        r#"interface I { const char C = '\a'; }"#,
        r#"interface I { const char C = '\f'; }"#,
        r#"interface I { const char C = '\v'; }"#,
        r#"interface I { const char C = '\b'; }"#,
    ] {
        let err = expect_parse_error(src, "test.aidl");
        assert!(matches!(&err, AidlError::Parse(_)), "src: {src}");
    }
    for src in [
        r#"interface I { const char C = '\n'; }"#,
        r#"interface I { const char C = '\t'; }"#,
        r#"interface I { const char C = '\0'; }"#,
        r#"interface I { const char C = 'A'; }"#,
    ] {
        let ctx = SourceContext::new("test.aidl", src);
        assert!(
            parse_document(&ctx).is_ok(),
            "supported char literal must still parse: {src}"
        );
    }
}

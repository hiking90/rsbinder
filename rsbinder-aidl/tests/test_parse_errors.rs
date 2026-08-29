// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Parse diagnostics: AIDL syntax errors must return `Err` with a usable
//! span and message, never panic.

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

// Missing semicolon
#[test]
fn test_missing_semicolon() {
    let err = expect_parse_error("parcelable Foo {\n    int field\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
    if let AidlError::Parse(pe) = &err {
        assert_eq!(pe.code().unwrap().to_string(), "aidl::parse_error");
    }
}

// Completely invalid input
#[test]
fn test_completely_invalid_input() {
    let err = expect_parse_error("this is not valid aidl at all", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
    if let AidlError::Parse(pe) = &err {
        assert_eq!(pe.code().unwrap().to_string(), "aidl::parse_error");
    }
}

// Empty document (no declarations, only package)
#[test]
fn test_empty_document() {
    let err = expect_parse_error("package android.test;", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Keyword used as identifier
#[test]
fn test_keyword_as_identifier() {
    let err = expect_parse_error("parcelable interface {\n    int val;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Unclosed brace
#[test]
fn test_unclosed_brace() {
    let err = expect_parse_error("parcelable Foo {\n    int val;\n", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Invalid type syntax (digits before type name)
#[test]
fn test_invalid_type_syntax() {
    let err = expect_parse_error("parcelable Foo {\n    123int val;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Duplicate package declaration
#[test]
fn test_duplicate_package() {
    let err = expect_parse_error("package a;\npackage b;\nparcelable Foo {}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Missing method parentheses
#[test]
fn test_missing_method_parens() {
    let err = expect_parse_error("interface IFoo {\n    void method;\n}", "test.aidl");
    assert!(matches!(&err, AidlError::Parse(_)));
}

// Error includes filename in source context
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

// Error span points to correct location
#[test]
fn test_error_span_points_to_correct_location() {
    let input = "parcelable Foo {\n    int 123bad;\n}";
    let err = expect_parse_error(input, "test.aidl");
    if let AidlError::Parse(pe) = &err {
        let labels: Vec<_> = pe.labels().expect("must have labels").collect();
        assert!(!labels.is_empty(), "must have at least one label");
        // `offset <= len` holds structurally, so it would still pass if every
        // diagnostic collapsed onto byte 0. Pin the actual location.
        let offset = labels[0].inner().offset();
        let bad = input
            .find("123bad")
            .expect("fixture contains the bad token");
        assert!(
            (bad..bad + "123bad".len()).contains(&offset),
            "span offset {offset} should point at `123bad` (byte {bad})"
        );
    } else {
        panic!("Expected AidlError::Parse, got: {err:?}");
    }
}

// A backslash or control byte in a String constant would be
// emitted verbatim into the generated Rust `"..."` and fail to compile (a raw
// `\X` is not necessarily a valid Rust escape; rsbinder does not decode string
// escapes). They are rejected at parse time. Non-ASCII (UTF-8) text stays
// valid — rsbinder is intentionally more lenient than AOSP there.
#[test]
fn test_string_constant_backslash_or_control_rejected() {
    for src in [
        r#"parcelable P { const String S = "a\nb"; }"#, // backslash-n (not decoded)
        r#"parcelable P { const String S = "a\\b"; }"#, // literal backslash
        r#"parcelable P { const String S = "a\"b"; }"#, // escaped quote
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

// An unsupported char escape must be rejected, not fall through to the
// post-backslash char verbatim — that silently yields the wrong code point
// (`'\a'` -> 'a' = 97, not bell = 7). Supported escapes still decode and a
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

/// `>>` closes two open generics; counting it only as a shift made
/// `angle_depth` accumulate across a statement, so a signature with enough
/// `List<List<T>>` arguments was rejected at a real nesting depth of 2.
#[test]
fn closing_shift_token_does_not_accumulate_generic_depth() {
    for n in [128usize, 400] {
        let args: Vec<String> = (0..n).map(|i| format!("in List<List<int>> a{i}")).collect();
        let src = format!("package a; interface I {{ void f({}); }}", args.join(", "));
        let ctx = SourceContext::new("test.aidl", &src);
        assert!(
            parse_document(&ctx).is_ok(),
            "{n} arguments nest only two deep and must parse"
        );
    }
}

/// The generic-nesting guard is far tighter than the bracket guard because
/// the parser's cost is exponential in generic depth, and the diagnostic must
/// name the limit it actually hit.
#[test]
fn deep_generic_nesting_is_rejected_with_a_naming_diagnostic() {
    let deep = format!(
        "package a; parcelable P {{ {}int{} x; }}",
        "Map<int, ".repeat(20),
        ">".repeat(20)
    );
    let err = expect_parse_error(&deep, "test.aidl");
    let AidlError::Parse(pe) = &err else {
        panic!("expected a ParseError, got: {err:?}");
    };
    assert!(pe.help().is_some(), "nesting diagnostic must carry help");
    assert!(
        pe.message.contains("generic types are nested too deeply"),
        "the diagnostic must name the limit it hit, got: {}",
        pe.message
    );

    // A long unbracketed operator chain trips a different limit, and must say so.
    let ops = format!(
        "package a; interface I {{ const int C = {}1; void f(); }}",
        "1+".repeat(2000)
    );
    let err = expect_parse_error(&ops, "test.aidl");
    let AidlError::Parse(pe) = &err else {
        panic!("expected a ParseError, got: {err:?}");
    };
    assert!(pe.help().is_some(), "nesting diagnostic must carry help");
    assert!(
        pe.message.contains("too many operators in one expression"),
        "an operator-run rejection must not be reported as nesting: {}",
        pe.message
    );

    // A shallow generic still parses.
    let ok = format!(
        "package a; parcelable P {{ {}int{} x; }}",
        "Map<int, ".repeat(3),
        ">".repeat(3)
    );
    assert!(parse_document(&SourceContext::new("test.aidl", &ok)).is_ok());
}

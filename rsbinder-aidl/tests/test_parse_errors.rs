// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Phase 2 parse error tests (작업 5.2)
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
    let err = expect_parse_error(
        "parcelable interface {\n    int val;\n}",
        "test.aidl",
    );
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
    let err = expect_parse_error(
        "package a;\npackage b;\nparcelable Foo {}",
        "test.aidl",
    );
    assert!(matches!(&err, AidlError::Parse(_)));
}

// 2.1h: Missing method parentheses
#[test]
fn test_missing_method_parens() {
    let err = expect_parse_error(
        "interface IFoo {\n    void method;\n}",
        "test.aidl",
    );
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
        assert!(offset <= input.len(), "span offset {offset} exceeds input length {}", input.len());
    } else {
        panic!("Expected AidlError::Parse, got: {err:?}");
    }
}

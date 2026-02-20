// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Phase 5 final integration tests (task 5.5)
//! Validates miette error output format and boundary conditions.

use std::error::Error;

use miette::{Diagnostic, GraphicalReportHandler, GraphicalTheme};
use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};

/// Render an AidlError using miette's GraphicalReportHandler (no color)
fn render_error(err: &AidlError) -> String {
    let mut buf = String::new();
    GraphicalReportHandler::new()
        .with_theme(GraphicalTheme::unicode_nocolor())
        .render_report(&mut buf, err as &dyn Diagnostic)
        .unwrap();
    buf
}

/// Helper: expect a parse-phase error
fn expect_parse_error(input: &str, filename: &str) -> AidlError {
    let ctx = SourceContext::new(filename, input);
    match parse_document(&ctx) {
        Err(e) => e,
        Ok(_) => panic!("Expected parse error but parsing succeeded"),
    }
}

/// Helper: expect a generation-phase error
fn expect_generation_error(input: &str, filename: &str) -> AidlError {
    let ctx = SourceContext::new(filename, input);
    let doc = parse_document(&ctx).expect("parsing should succeed for this test");
    let gen = Generator::new(false, false);
    match gen.document(&doc) {
        Err(e) => e,
        Ok(_) => panic!("Expected generation error but generation succeeded"),
    }
}

// ============================================================
// 5.5a~5.5e: Error output format verification
// ============================================================

// 5.5a: Parse error output includes filename, source snippet, diagnostic code
#[test]
fn test_parse_error_output_format() {
    let err = expect_parse_error(
        "interface IHello {\n    void 123bad();\n}",
        "hello.aidl",
    );
    let rendered = render_error(&err);
    assert!(
        rendered.contains("hello.aidl"),
        "Should contain filename:\n{rendered}"
    );
    assert!(
        rendered.contains("aidl::parse_error"),
        "Should contain diagnostic code:\n{rendered}"
    );
}

// 5.5b: Semantic error output shows duplicate transaction code info
#[test]
fn test_semantic_error_output_format() {
    let err = expect_generation_error(
        r#"
interface IDup {
    void m1() = 10;
    void m2() = 10;
}
        "#,
        "test.aidl",
    );
    let rendered = render_error(&err);
    assert!(
        rendered.contains("aidl::duplicate_transaction_code"),
        "Should contain diagnostic code:\n{rendered}"
    );
    assert!(
        rendered.contains("m1") || rendered.contains("m2"),
        "Should mention method names:\n{rendered}"
    );
}

// 5.5c: Multiple errors output format
#[test]
fn test_multiple_errors_output_format() {
    // Create a Multiple error manually for output format testing
    let err1 = expect_parse_error("invalid aidl 1", "file1.aidl");
    let err2 = expect_parse_error("invalid aidl 2", "file2.aidl");
    let multi = AidlError::Multiple {
        errors: vec![err1, err2],
    };
    let rendered = render_error(&multi);
    assert!(
        rendered.contains("error(s) occurred"),
        "Should mention error count:\n{rendered}"
    );
}

// 5.5d: Error output includes help section
#[test]
fn test_error_output_includes_help() {
    let err = expect_generation_error(
        r#"
parcelable Foo {
    @nullable int val;
}
        "#,
        "test.aidl",
    );
    let rendered = render_error(&err);
    // The error should have some diagnostic info (help or label)
    assert!(
        !rendered.is_empty(),
        "Rendered output should not be empty"
    );
}

// 5.5e: Error output includes source snippet
#[test]
fn test_error_output_includes_source_snippet() {
    let input = "parcelable Foo {\n    int field\n}";
    let err = expect_parse_error(input, "test.aidl");
    let rendered = render_error(&err);
    // The rendered output should contain some part of the source
    assert!(
        rendered.contains("int") || rendered.contains("field") || rendered.contains("parcelable"),
        "Should contain source snippet:\n{rendered}"
    );
}

// ============================================================
// 5.5k~5.5r: Boundary condition tests
// ============================================================

// 5.5k: Empty input
#[test]
fn test_empty_input() {
    let ctx = SourceContext::new("test.aidl", "");
    let result = parse_document(&ctx);
    assert!(result.is_err(), "Empty input should fail");
}

// 5.5l: Whitespace only input
#[test]
fn test_whitespace_only_input() {
    let ctx = SourceContext::new("test.aidl", "   \n\n  ");
    let result = parse_document(&ctx);
    assert!(result.is_err(), "Whitespace-only input should fail");
}

// 5.5m: Comment only input (no declarations)
#[test]
fn test_comment_only_input() {
    let ctx = SourceContext::new("test.aidl", "// just a comment\n/* block */\n");
    let result = parse_document(&ctx);
    assert!(result.is_err(), "Comment-only input should fail");
}

// 5.5n: Very long identifier (should not crash)
#[test]
fn test_very_long_identifier() {
    let long_name = "A".repeat(10000);
    let input = format!("parcelable {long_name} {{}}");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", &input);
        let _ = parse_document(&ctx);
    }));
    assert!(result.is_ok(), "Very long identifier should not crash");
}

// 5.5o: Deeply nested const expression
#[test]
fn test_deeply_nested_const_expr() -> Result<(), Box<dyn Error>> {
    let input = r#"
parcelable Foo {
    const int V = ((((((((1+2)+3)+4)+5)+6)+7)+8)+9);
}
    "#;
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx)?;
    let gen = Generator::new(false, false);
    gen.document(&doc)?;
    Ok(())
}

// 5.5p: Unicode in string constant
#[test]
fn test_unicode_in_string_constant() -> Result<(), Box<dyn Error>> {
    let input = "parcelable Foo {\n    const String MSG = \"한글 테스트\";\n}";
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx)?;
    let gen = Generator::new(false, false);
    gen.document(&doc)?;
    Ok(())
}

// 5.5q: Transaction code at u32::MAX should succeed
#[test]
fn test_transaction_code_u32_max() -> Result<(), Box<dyn Error>> {
    let input = r#"
interface IFoo {
    void m1() = 4294967295;
    void m2() = 4294967294;
}
    "#;
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx)?;
    let gen = Generator::new(false, false);
    gen.document(&doc)?;
    Ok(())
}

// 5.5r: Transaction code at u32::MAX + 1 should fail
#[test]
fn test_transaction_code_u32_max_plus_one() {
    let err = expect_generation_error(
        r#"
interface IFoo {
    void m1() = 4294967296;
    void m2() = 4294967295;
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("overflow") || msg.contains("u32") || msg.contains("exceeding"),
        "Error should mention overflow: {msg}"
    );
}

// ============================================================
// 2.6: fancy / non-fancy / ascii output format verification
// ============================================================

// Fancy mode: GraphicalTheme::unicode() — includes ANSI color codes
#[test]
fn test_fancy_mode_output() {
    let err = expect_parse_error(
        "interface IHello {\n    void 123bad();\n}",
        "hello.aidl",
    );
    let mut buf = String::new();
    GraphicalReportHandler::new()
        .with_theme(GraphicalTheme::unicode())
        .render_report(&mut buf, &err as &dyn Diagnostic)
        .unwrap();

    assert!(!buf.is_empty(), "Fancy mode output must not be empty");
    assert!(
        buf.contains("hello.aidl"),
        "Fancy mode must contain filename:\n{buf}"
    );
    assert!(
        buf.contains("aidl::parse_error"),
        "Fancy mode must contain diagnostic code:\n{buf}"
    );
    // Colored mode must contain ESC sequences ('\x1b[')
    assert!(
        buf.contains('\x1b'),
        "Fancy (colored) mode must contain ANSI escape codes:\n{buf}"
    );
}

// Non-fancy mode: GraphicalTheme::unicode_nocolor() — no ANSI color codes
#[test]
fn test_non_fancy_mode_output() {
    let err = expect_parse_error(
        "interface IHello {\n    void 123bad();\n}",
        "hello.aidl",
    );
    // The render_error() helper already uses unicode_nocolor()
    let rendered = render_error(&err);

    assert!(!rendered.is_empty(), "Non-fancy mode output must not be empty");
    assert!(
        rendered.contains("hello.aidl"),
        "Non-fancy mode must contain filename:\n{rendered}"
    );
    assert!(
        rendered.contains("aidl::parse_error"),
        "Non-fancy mode must contain diagnostic code:\n{rendered}"
    );
    // No-color mode must not contain any ANSI escape codes
    assert!(
        !rendered.contains('\x1b'),
        "Non-fancy (no-color) mode must not contain ANSI escape codes:\n{rendered}"
    );
}

// ASCII mode: GraphicalTheme::ascii() — pure ASCII without Unicode box-drawing characters
#[test]
fn test_ascii_mode_output() {
    let err = expect_parse_error(
        "parcelable Foo {\n    int field\n}",
        "foo.aidl",
    );
    let mut buf = String::new();
    GraphicalReportHandler::new()
        .with_theme(GraphicalTheme::ascii())
        .render_report(&mut buf, &err as &dyn Diagnostic)
        .unwrap();

    assert!(!buf.is_empty(), "ASCII mode output must not be empty");
    assert!(
        buf.contains("foo.aidl"),
        "ASCII mode must contain filename:\n{buf}"
    );
    assert!(
        buf.contains("aidl::parse_error"),
        "ASCII mode must contain diagnostic code:\n{buf}"
    );
    // ASCII mode output must consist entirely of ASCII characters
    assert!(
        buf.is_ascii(),
        "ASCII mode output must contain only ASCII characters"
    );
}

// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Syntax errors return `Err` with a usable span and message, never panic.

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

// AOSP `ParseInt` on the token text: a suffix or separator is a parse error, never a typed literal.
#[test]
fn test_transaction_code_is_a_plain_decimal() {
    for code in ["200u8", "3L", "1_0"] {
        let input = format!("interface IFoo {{\n    void m() = {code};\n}}");
        let err = expect_parse_error(&input, "test.aidl");
        let AidlError::Parse(pe) = &err else {
            panic!("expected a parse error for `{code}`, got {err:?}");
        };
        assert_eq!(pe.message, format!("Could not parse int value: {code}"));
    }
    let ctx = SourceContext::new("test.aidl", "interface IFoo {\n    void m() = 200;\n}");
    parse_document(&ctx).expect("a plain decimal is accepted");
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
        // `offset <= len` holds even if every span collapsed to byte 0: pin the location.
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

// `\` and control bytes would break the emitted Rust literal; non-ASCII stays (laxer than AOSP).
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

// Unsupported char escapes are rejected: a fall-through would make `'\a'` 97, not bell (7).
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

/// Comparison chains and bracket-split runs hit the operator cap instead of overflowing the stack.
#[test]
fn operator_runs_through_comparisons_and_brackets_are_rejected() {
    // 200 bracket levels (under the bracket cap) of 50 operators each: one path of 10000.
    let level = format!("({}", "1+".repeat(50));
    let nested = format!("{}1{}", level.repeat(200), ")".repeat(200));
    for expr in [
        format!("{}1", "1 == ".repeat(200_000)),
        format!("{}1", "1 != ".repeat(200_000)),
        format!("{}1", "1 < ".repeat(200_000)),
        format!("{}1", "1 <= ".repeat(200_000)),
        format!("{}1", "1 > ".repeat(200_000)),
        format!("{}1", "1 >= ".repeat(200_000)),
        format!("{}1", "(1)+".repeat(200_000)),
        nested,
        format!("{}A", "A < A > ".repeat(100_000)),
        format!("{}A", "A<A<A>>".repeat(100_000)),
    ] {
        let src = format!("parcelable P {{ const int X = {expr}; }}");
        // Default test-thread stack: a build script's main thread is no smaller.
        let result = std::thread::spawn(move || {
            parse_document(&SourceContext::new("test.aidl", &src)).map(|_| ())
        })
        .join()
        .expect("parsing must not panic");
        let err = result.expect_err("an unbounded operator run must be refused");
        let AidlError::Parse(pe) = &err else {
            panic!("expected a ParseError, got: {err:?}");
        };
        assert!(
            pe.message.contains("too many operators in one expression"),
            "got: {}",
            pe.message
        );
        assert!(pe.help().is_some(), "nesting diagnostic must carry help");
    }
}

/// `>>` closes two generics; as a bare shift, `angle_open` would accumulate across arguments.
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

/// Generic depth is capped below brackets (parse cost is exponential in it); errors name the cap.
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

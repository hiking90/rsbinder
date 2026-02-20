// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Phase 3 semantic error tests (작업 5.3)
//! Validates transaction code errors, import resolution errors, and error aggregation.

use miette::Diagnostic;
use rsbinder_aidl::error::SemanticError;
use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};

/// Helper: parse + generate, expect generation-phase error
fn expect_generation_error(input: &str, filename: &str) -> AidlError {
    let ctx = SourceContext::new(filename, input);
    let doc = parse_document(&ctx).expect("parsing should succeed for this test");
    let gen = Generator::new(false, false);
    match gen.document(&doc) {
        Err(e) => e,
        Ok(_) => panic!("Expected generation error but generation succeeded"),
    }
}

// 3.1a: Mixed explicit/implicit transaction IDs
#[test]
fn test_mixed_transaction_ids() {
    let err = expect_generation_error(
        r#"
interface IMixed {
    void method1() = 10;
    void method2();
}
        "#,
        "test.aidl",
    );
    match &err {
        AidlError::Semantic(SemanticError::MixedTransactionIds { interface, .. }) => {
            assert_eq!(interface, "IMixed");
        }
        other => panic!("Expected MixedTransactionIds, got: {other}"),
    }
    if let AidlError::Semantic(se) = &err {
        assert_eq!(se.code().unwrap().to_string(), "aidl::mixed_transaction_ids");
    }
}

// 3.1b: Duplicate transaction codes
#[test]
fn test_duplicate_transaction_codes() {
    let err = expect_generation_error(
        r#"
interface IDup {
    void m1() = 10;
    void m2() = 10;
}
        "#,
        "test.aidl",
    );
    match &err {
        AidlError::Semantic(SemanticError::DuplicateTransactionCode {
            method1,
            method2,
            code,
            ..
        }) => {
            assert_eq!(*code, 10);
            // One of m1/m2 should be method1, the other method2
            assert!(
                (method1 == "m1" && method2 == "m2") || (method1 == "m2" && method2 == "m1"),
                "Expected m1 and m2, got: {method1}, {method2}"
            );
        }
        other => panic!("Expected DuplicateTransactionCode, got: {other}"),
    }
}

// 3.1c: Transaction code exceeds u32::MAX
#[test]
fn test_transaction_code_u32_overflow() {
    let err = expect_generation_error(
        r#"
interface IOver {
    void m1() = 9999999999;
    void m2() = 9999999998;
}
        "#,
        "test.aidl",
    );
    match &err {
        AidlError::Semantic(SemanticError::TransactionCodeOverflow { code, .. }) => {
            assert!(*code > u32::MAX as i64);
        }
        other => panic!("Expected TransactionCodeOverflow, got: {other}"),
    }
    if let AidlError::Semantic(se) = &err {
        assert_eq!(
            se.code().unwrap().to_string(),
            "aidl::transaction_code_overflow"
        );
    }
}

// 3.1d: DuplicateTransactionCode span points to method identifiers
#[test]
fn test_duplicate_code_span_points_to_methods() {
    let err = expect_generation_error(
        r#"
interface IDup {
    void m1() = 10;
    void m2() = 10;
}
        "#,
        "test.aidl",
    );
    if let AidlError::Semantic(SemanticError::DuplicateTransactionCode {
        span, related, ..
    }) = &err
    {
        // span should be non-zero (pointing to first method)
        assert!(!span.is_empty() || span.offset() > 0, "span should point to a method identifier");
        // related should have exactly 1 entry (the second method)
        assert_eq!(related.len(), 1, "expected 1 related diagnostic");
        let related_labels: Vec<_> = related[0]
            .labels()
            .expect("related must have labels")
            .collect();
        assert!(!related_labels.is_empty());
    } else {
        panic!("Expected DuplicateTransactionCode, got: {err}");
    }
}

// 3.1e: MixedTransactionIds span points to interface name
#[test]
fn test_mixed_ids_span_points_to_interface() {
    let input = r#"
interface IMixed {
    void method1() = 10;
    void method2();
}
    "#;
    let err = expect_generation_error(input, "test.aidl");
    if let AidlError::Semantic(SemanticError::MixedTransactionIds { span, .. }) = &err {
        // span should point to the interface name "IMixed"
        let offset = span.offset();
        let len = span.len();
        assert!(len > 0, "span should have non-zero length");
        // Verify the span covers "IMixed" in the source
        let spanned_text = &input[offset..offset + len];
        assert_eq!(spanned_text, "IMixed", "span should cover interface name");
    } else {
        panic!("Expected MixedTransactionIds, got: {err}");
    }
}

// 3.2a: Import not found (using Builder with temp file)
#[test]
fn test_import_not_found() {
    let tmp = std::env::temp_dir().join("rsbinder_test_import_not_found");
    std::fs::create_dir_all(&tmp).unwrap();
    let aidl_path = tmp.join("Foo.aidl");
    std::fs::write(
        &aidl_path,
        "import foo.bar.NonExistent;\nparcelable Foo {}",
    )
    .unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(&aidl_path)
        .output(&tmp)
        .generate();

    assert!(result.is_err(), "Expected import not found error");
    let err = result.unwrap_err();
    let err_msg = format!("{err}");
    assert!(
        err_msg.contains("not found") || err_msg.contains("import"),
        "Error message should mention import: {err_msg}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

// 3.2b: ImportNotFound includes help message
#[test]
fn test_import_not_found_includes_help() {
    let tmp = std::env::temp_dir().join("rsbinder_test_import_help");
    std::fs::create_dir_all(&tmp).unwrap();
    let aidl_path = tmp.join("Bar.aidl");
    std::fs::write(
        &aidl_path,
        "import nonexistent.Type;\nparcelable Bar {}",
    )
    .unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(&aidl_path)
        .output(&tmp)
        .generate();

    assert!(result.is_err());
    let err = result.unwrap_err();
    if let AidlError::Resolution(ref re) = err {
        let help = re.help().map(|h| h.to_string());
        assert!(
            help.is_some(),
            "ImportNotFound should have a help message"
        );
        assert!(
            help.unwrap().contains("include paths"),
            "help should mention include paths"
        );
    }
    // Note: error might be wrapped in Multiple, so also check the string form
    let rendered = format!("{err:?}");
    assert!(
        rendered.contains("not found") || rendered.contains("import"),
        "Should mention import error: {rendered}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

// 3.3a: Multiple file errors collected
#[test]
fn test_multiple_file_errors_collected() {
    let tmp = std::env::temp_dir().join("rsbinder_test_multiple_errors");
    std::fs::create_dir_all(&tmp).unwrap();

    // Two files, each with an invalid import
    std::fs::write(
        tmp.join("A.aidl"),
        "import nonexistent.TypeA;\nparcelable A {}",
    )
    .unwrap();
    std::fs::write(
        tmp.join("B.aidl"),
        "import nonexistent.TypeB;\nparcelable B {}",
    )
    .unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(tmp.join("A.aidl"))
        .source(tmp.join("B.aidl"))
        .output(&tmp)
        .generate();

    assert!(result.is_err(), "Expected errors from both files");
    let err = result.unwrap_err();
    let err_msg = format!("{err}");
    // Should contain error information (either Multiple or individual error)
    assert!(
        err_msg.contains("not found") || err_msg.contains("error"),
        "Error should mention the issues: {err_msg}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

// 3.3b: Single file with one error — not wrapped in AidlError::Multiple
#[test]
fn test_single_file_error_not_wrapped() {
    let tmp = std::env::temp_dir().join("rsbinder_test_single_not_multiple");
    std::fs::create_dir_all(&tmp).unwrap();

    // One file with an import error, one file valid
    std::fs::write(
        tmp.join("Bad.aidl"),
        "import nonexistent.TypeX;\nparcelable Bad {}",
    )
    .unwrap();
    std::fs::write(tmp.join("Good.aidl"), "parcelable Good {}").unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(tmp.join("Bad.aidl"))
        .source(tmp.join("Good.aidl"))
        .output(&tmp)
        .generate();

    assert!(result.is_err(), "Expected error from Bad.aidl");
    let err = result.unwrap_err();
    // AidlError::collect() unwraps a vec of length 1 — must NOT be Multiple
    assert!(
        !matches!(err, AidlError::Multiple { .. }),
        "Single import error should not be wrapped in Multiple, got: {err}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

// 3.3c: Parse error in file A blocks semantic analysis of file B (cascading error prevention)
#[test]
fn test_parse_error_blocks_semantic_analysis() {
    let tmp = std::env::temp_dir().join("rsbinder_test_cascade_prevention");
    std::fs::create_dir_all(&tmp).unwrap();

    // A.aidl: intentional syntax error (missing semicolon after field)
    std::fs::write(
        tmp.join("A.aidl"),
        "package test;\nparcelable A {\n    int field\n}",
    )
    .unwrap();

    // B.aidl: syntactically valid, imports and uses A's type.
    // Without cascading prevention this would also fail with UnknownType at generation.
    std::fs::write(
        tmp.join("B.aidl"),
        "package test;\nimport test.A;\nparcelable B {\n    A item;\n}",
    )
    .unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(tmp.join("A.aidl"))
        .source(tmp.join("B.aidl"))
        .include_dir(&tmp)
        .output(&tmp)
        .generate();

    assert!(result.is_err(), "Expected parse error from A.aidl");
    let err = result.unwrap_err();

    // Cascading prevention: only A's ParseError should be reported.
    // B's generation-phase SemanticError::UnknownType must NOT appear.
    let contains_semantic = match &err {
        AidlError::Semantic(_) => true,
        AidlError::Multiple { errors } => {
            errors.iter().any(|e| matches!(e, AidlError::Semantic(_)))
        }
        _ => false,
    };
    assert!(
        !contains_semantic,
        "Cascading SemanticError from B should be suppressed when A fails to parse: {err}"
    );

    std::fs::remove_dir_all(&tmp).ok();
}

// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Semantic diagnostics: transaction code errors, import resolution errors,
//! and multi-file error aggregation.

use miette::Diagnostic;
use rsbinder_aidl::error::SemanticError;
use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};
use std::path::PathBuf;

/// A fresh, per-test directory under the target dir — `std::env::temp_dir()`
/// is shared, so two concurrent `cargo test` runs would delete each other's
/// fixtures mid-generation.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

// Mixed explicit/implicit transaction IDs
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
        AidlError::Semantic(se) => match se.as_ref() {
            SemanticError::MixedTransactionIds { interface, .. } => {
                assert_eq!(interface, "IMixed");
            }
            other => panic!("Expected MixedTransactionIds, got: {other}"),
        },
        other => panic!("Expected MixedTransactionIds, got: {other}"),
    }
    if let AidlError::Semantic(se) = &err {
        assert_eq!(
            se.code().unwrap().to_string(),
            "aidl::mixed_transaction_ids"
        );
    }
}

// Duplicate transaction codes
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
        AidlError::Semantic(se) => match se.as_ref() {
            SemanticError::DuplicateTransactionCode {
                method1,
                method2,
                code,
                ..
            } => {
                assert_eq!(*code, 10);
                // One of m1/m2 should be method1, the other method2
                assert!(
                    (method1 == "m1" && method2 == "m2") || (method1 == "m2" && method2 == "m1"),
                    "Expected m1 and m2, got: {method1}, {method2}"
                );
            }
            other => panic!("Expected DuplicateTransactionCode, got: {other}"),
        },
        other => panic!("Expected DuplicateTransactionCode, got: {other}"),
    }
}

// Transaction code exceeds u32::MAX
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
        AidlError::Semantic(se) => match se.as_ref() {
            SemanticError::TransactionCodeOverflow { code, .. } => {
                assert!(*code > u32::MAX as i64);
            }
            other => panic!("Expected TransactionCodeOverflow, got: {other}"),
        },
        other => panic!("Expected TransactionCodeOverflow, got: {other}"),
    }
    if let AidlError::Semantic(se) = &err {
        assert_eq!(
            se.code().unwrap().to_string(),
            "aidl::transaction_code_overflow"
        );
    }
}

// DuplicateTransactionCode span points to method identifiers
#[test]
fn test_duplicate_code_span_points_to_methods() {
    let input = r#"
interface IDup {
    void m1() = 10;
    void m2() = 10;
}
        "#;
    let err = expect_generation_error(input, "test.aidl");
    if let AidlError::Semantic(se) = &err {
        if let SemanticError::DuplicateTransactionCode { span, related, .. } = se.as_ref() {
            let pointed = &input[span.offset()..span.offset() + span.len()];
            assert_eq!(
                pointed, "m1",
                "span must cover the first colliding method identifier"
            );
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
    } else {
        panic!("Expected Semantic error, got: {err}");
    }
}

// MixedTransactionIds span points to interface name
#[test]
fn test_mixed_ids_span_points_to_interface() {
    let input = r#"
interface IMixed {
    void method1() = 10;
    void method2();
}
    "#;
    let err = expect_generation_error(input, "test.aidl");
    if let AidlError::Semantic(se) = &err {
        if let SemanticError::MixedTransactionIds { span, .. } = se.as_ref() {
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
    } else {
        panic!("Expected Semantic error, got: {err}");
    }
}

// Import not found (using Builder with temp file)
#[test]
fn test_import_not_found() {
    let tmp = scratch_dir("import_not_found");
    let aidl_path = tmp.join("Foo.aidl");
    std::fs::write(&aidl_path, "import foo.bar.NonExistent;\nparcelable Foo {}").unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(&aidl_path)
        .output(&tmp)
        .generate();

    let err = result.expect_err("Expected import not found error");
    assert_eq!(
        err.to_string(),
        "import 'foo.bar.NonExistent' not found",
        "got: {err:?}"
    );
}

// ImportNotFound includes help message
#[test]
fn test_import_not_found_includes_help() {
    let tmp = scratch_dir("import_help");
    let aidl_path = tmp.join("Bar.aidl");
    std::fs::write(&aidl_path, "import nonexistent.Type;\nparcelable Bar {}").unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(&aidl_path)
        .output(&tmp)
        .generate();

    let err = result.expect_err("Expected import not found error");
    let AidlError::Resolution(re) = &err else {
        panic!("a single import failure must stay unwrapped, got: {err:?}");
    };
    let help = re.help().expect("ImportNotFound must carry a help message");
    assert!(
        help.to_string().contains("include paths"),
        "help should mention include paths: {help}"
    );
    assert_eq!(
        err.to_string(),
        "import 'nonexistent.Type' not found",
        "got: {err:?}"
    );
}

// Multiple file errors collected
#[test]
fn test_multiple_file_errors_collected() {
    let tmp = scratch_dir("multiple_errors");

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

    let err = result.expect_err("Expected errors from both files");
    let AidlError::Multiple { errors } = &err else {
        panic!("two failing files must aggregate into Multiple, got: {err:?}");
    };
    // Import resolution walks a HashMap, so pin the set, not the order.
    let mut messages: Vec<String> = errors.iter().map(|e| e.to_string()).collect();
    messages.sort();
    assert_eq!(
        messages,
        vec![
            "import 'nonexistent.TypeA' not found".to_string(),
            "import 'nonexistent.TypeB' not found".to_string(),
        ],
        "one ImportNotFound per file: {err:?}"
    );
}

// Single file with one error — not wrapped in AidlError::Multiple
#[test]
fn test_single_file_error_not_wrapped() {
    let tmp = scratch_dir("single_not_multiple");

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
}

// Parse error in file A blocks semantic analysis of file B (cascading error prevention)
#[test]
fn test_parse_error_blocks_semantic_analysis() {
    let tmp = scratch_dir("cascade_prevention");

    let pkg = tmp.join("test");
    std::fs::create_dir_all(&pkg).unwrap();

    // A.aidl: intentional syntax error (missing semicolon after field)
    std::fs::write(
        pkg.join("A.aidl"),
        "package test;\nparcelable A {\n    int field\n}",
    )
    .unwrap();

    // B.aidl: syntactically valid, imports and uses A's type.
    // Without cascading prevention this would also fail with UnknownType at generation.
    std::fs::write(
        pkg.join("B.aidl"),
        "package test;\nimport test.A;\nparcelable B {\n    A item;\n}",
    )
    .unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(pkg.join("A.aidl"))
        .source(pkg.join("B.aidl"))
        .include_dir(&tmp)
        .output(&tmp)
        .generate();

    assert!(result.is_err(), "Expected parse error from A.aidl");
    let err = result.unwrap_err();

    // Cascading prevention: only A's ParseError may be reported. B's
    // generation-phase `UnknownType` is a `ResolutionError`, so matching on
    // `AidlError::Semantic` alone would never observe it.
    let reported: Vec<&AidlError> = match &err {
        AidlError::Multiple { errors } => errors.iter().collect(),
        single => vec![single],
    };
    assert!(
        reported.iter().all(|e| matches!(e, AidlError::Parse(_))),
        "only A's ParseError may be reported, got: {err:?}"
    );
    assert!(
        !format!("{err:?}").contains("UnknownType"),
        "B's generation-phase UnknownType must be suppressed: {err:?}"
    );
}

// ── direction_span location verification ─────────────────────────────────────

// direction_span: verify that the span points to the 'out' keyword
#[test]
fn test_out_primitive_span_points_to_out_keyword() {
    let input = "interface IFoo {\n    void foo(out int x);\n}";
    let err = expect_generation_error(input, "test.aidl");
    if let AidlError::Semantic(se) = &err {
        if let SemanticError::DirectionPrimitive {
            span,
            direction,
            type_kind,
            help,
            ..
        } = se.as_ref()
        {
            let offset = span.offset();
            let len = span.len();
            assert!(len > 0, "span should have non-zero length");
            let spanned_text = &input[offset..offset + len];
            assert_eq!(
                spanned_text, "out",
                "span should point to 'out' keyword, got: '{spanned_text}'"
            );
            assert_eq!(direction, "out");
            assert_eq!(type_kind, "a primitive type");
            assert!(
                help.as_deref().unwrap_or("").contains("remove 'out'"),
                "help should suggest removing the 'out' keyword, got: {help:?}"
            );
        } else {
            panic!("Expected DirectionPrimitive, got: {err}");
        }
    } else {
        panic!("Expected Semantic error, got: {err}");
    }
}

// direction_span: verify that the span points to the 'inout' keyword
#[test]
fn test_inout_string_span_points_to_inout_keyword() {
    let input = "interface IFoo {\n    void bar(inout String s);\n}";
    let err = expect_generation_error(input, "test.aidl");
    if let AidlError::Semantic(se) = &err {
        if let SemanticError::DirectionPrimitive {
            span,
            direction,
            type_kind,
            help,
            ..
        } = se.as_ref()
        {
            let offset = span.offset();
            let len = span.len();
            assert!(len > 0, "span should have non-zero length");
            let spanned_text = &input[offset..offset + len];
            assert_eq!(
                spanned_text, "inout",
                "span should point to 'inout' keyword, got: '{spanned_text}'"
            );
            assert_eq!(direction, "inout");
            assert_eq!(type_kind, "String");
            assert!(
                help.as_deref().unwrap_or("").contains("remove 'inout'"),
                "help should suggest removing the 'inout' keyword, got: {help:?}"
            );
        } else {
            panic!("Expected DirectionPrimitive, got: {err}");
        }
    } else {
        panic!("Expected Semantic error, got: {err}");
    }
}

// ── @Backing(type=...) validation (AOSP allowlist: byte/int/long) ────────────

/// Helper: assert that `input` produces an InvalidBackingType diagnostic whose
/// label points exactly at the supplied `expected_annotation` source text.
fn assert_invalid_backing_type(input: &str, expected_type_name: &str, expected_annotation: &str) {
    let err = expect_generation_error(input, "test.aidl");
    let AidlError::Semantic(se) = &err else {
        panic!("Expected Semantic error, got: {err}");
    };
    let SemanticError::InvalidBackingType {
        type_name, span, ..
    } = se.as_ref()
    else {
        panic!("Expected InvalidBackingType, got: {err}");
    };
    assert_eq!(type_name, expected_type_name);

    let offset = span.offset();
    let len = span.len();
    assert!(len > 0, "span should have non-zero length");
    let spanned_text = &input[offset..offset + len];
    assert_eq!(
        spanned_text, expected_annotation,
        "span should cover the @Backing annotation, got: '{spanned_text}'"
    );

    // diagnostic code should be the dedicated one (not the generic invalid_operation)
    assert_eq!(se.code().unwrap().to_string(), "aidl::invalid_backing_type");
    // help text must enumerate the allowed AOSP backing types
    let help = se.help().map(|h| h.to_string()).unwrap_or_default();
    assert!(
        help.contains("byte") && help.contains("int") && help.contains("long"),
        "help should list allowed backing types, got: {help}"
    );
}

#[test]
fn test_invalid_backing_type_unknown_identifier() {
    assert_invalid_backing_type(
        "package foo;\n@Backing(type=\"NotAType\")\nenum MyEnum { V1 = 1 }",
        "NotAType",
        "@Backing(type=\"NotAType\")",
    );
}

#[test]
fn test_invalid_backing_type_string() {
    // String is a real AIDL/Rust type but not a valid enum backing per AOSP.
    assert_invalid_backing_type(
        "package foo;\n@Backing(type=\"String\")\nenum MyEnum { V1 = 1 }",
        "String",
        "@Backing(type=\"String\")",
    );
}

#[test]
fn test_invalid_backing_type_common_typo() {
    // 'integer' is a frequent typo for 'int' — the diagnostic must catch it.
    assert_invalid_backing_type(
        "package foo;\n@Backing(type=\"integer\")\nenum MyEnum { V1 = 1 }",
        "integer",
        "@Backing(type=\"integer\")",
    );
}

#[test]
fn test_invalid_backing_type_char_rejected() {
    // 'char' is a valid AIDL type but AOSP rejects it as enum backing.
    assert_invalid_backing_type(
        "package foo;\n@Backing(type=\"char\")\nenum MyEnum { V1 = 1 }",
        "char",
        "@Backing(type=\"char\")",
    );
}

#[test]
fn test_invalid_backing_type_list_replaces_old_invalid_operation() {
    // Prior behaviour: List backing hit TypeGenerator's "List must have
    // Generic Type" arm and surfaced as InvalidOperation. The dedicated
    // InvalidBackingType validation runs first, so List is now reported
    // with the AOSP-faithful diagnostic instead.
    assert_invalid_backing_type(
        "package foo;\n@Backing(type=\"List\")\nenum MyEnum { V1 = 1 }",
        "List",
        "@Backing(type=\"List\")",
    );
}

#[test]
fn test_valid_backing_types_generate_successfully() {
    for ty in ["byte", "int", "long"] {
        let input = format!("package foo;\n@Backing(type=\"{ty}\")\nenum MyEnum {{ V1 = 1 }}");
        let ctx = SourceContext::new("test.aidl", &input);
        let doc = parse_document(&ctx).expect("parse");
        let gen = Generator::new(false, false);
        let result = gen.document(&doc);
        assert!(
            result.is_ok(),
            "@Backing(type=\"{ty}\") should generate successfully, got: {:?}",
            result.err()
        );
    }
}

#[test]
fn test_no_backing_annotation_defaults_to_byte() {
    // The implicit-byte default must not regress into the new validator.
    let input = "package foo;\nenum MyEnum { V1 = 1 }";
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx).expect("parse");
    let gen = Generator::new(false, false);
    assert!(gen.document(&doc).is_ok());
}

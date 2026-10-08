// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Semantic diagnostics: transaction codes, import resolution, multi-file aggregation.

use miette::Diagnostic;
use rsbinder_aidl::error::SemanticError;
use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};
use std::path::PathBuf;

/// Per-test dir under the target dir: tests cannot clobber each other or the system temp dir.
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

// The "imported here" label lands on the import, not an earlier longer name sharing its prefix
#[test]
fn test_import_not_found_label_matches_the_whole_name() {
    let tmp = scratch_dir("import_label_whole_name");
    let aidl_path = tmp.join("Bar.aidl");
    let text = "import p.IFooBar;\nimport p.IFoo;\nparcelable Bar {}";
    std::fs::write(&aidl_path, text).unwrap();

    let err = rsbinder_aidl::Builder::new()
        .source(&aidl_path)
        .output(&tmp)
        .generate()
        .expect_err("both imports are missing");
    let AidlError::Multiple { errors } = &err else {
        panic!("two failing imports must aggregate into Multiple, got: {err:?}");
    };
    let short = errors
        .iter()
        .find(|e| e.to_string() == "import 'p.IFoo' not found")
        .unwrap_or_else(|| panic!("no p.IFoo error: {err:?}"));
    let label = short
        .labels()
        .and_then(|mut labels| labels.next())
        .expect("ImportNotFound carries a label");
    assert_eq!(label.offset(), text.find("p.IFoo;").unwrap(), "{err:?}");
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

    // B.aidl: valid, uses A's type; cascading prevention must suppress its UnknownType.
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

    // Only A's ParseError: B's `UnknownType` is a `ResolutionError`, not `AidlError::Semantic`.
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
        if let SemanticError::InvalidDirection {
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
            assert_eq!(type_kind, "int");
            assert!(
                help.as_deref().unwrap_or("").contains("remove 'out'"),
                "help should suggest removing the 'out' keyword, got: {help:?}"
            );
        } else {
            panic!("Expected InvalidDirection, got: {err}");
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
        if let SemanticError::InvalidDirection {
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
            panic!("Expected InvalidDirection, got: {err}");
        }
    } else {
        panic!("Expected Semantic error, got: {err}");
    }
}

// ── argument directions (AOSP `GetArgumentAspect` + `AidlArgument::CheckValid`) ─

/// Types the argument matrix below names, nested so one document resolves them.
const DIRECTION_DECLS: &str = "\
    parcelable P { int x; }
    parcelable G<T> { int x; }
    union U { int a; String b; }
    @JavaOnlyImmutable parcelable Imm { int x; }
    @FixedSize parcelable F { int x; }
    enum E { A, B }
    interface ICb { void ping(); }
";

fn generate_method(method: &str) -> Result<(), AidlError> {
    let input = format!("package test;\ninterface IFoo {{\n{DIRECTION_DECLS}    {method}\n}}");
    let ctx = SourceContext::new("test.aidl", &input);
    let doc = parse_document(&ctx).unwrap_or_else(|e| panic!("{method}: parse failed: {e}"));
    Generator::new(false, false).document(&doc).map(|_| ())
}

fn semantic(err: &AidlError) -> &SemanticError {
    match err {
        AidlError::Semantic(se) => se,
        other => panic!("Expected Semantic error, got: {other}"),
    }
}

/// AOSP `aidl_unittest.cpp:220`, `:4356`, `:5032` (directions outside the type's aspect).
#[test]
fn test_direction_outside_the_types_aspect_is_rejected() {
    let cases = [
        ("void f(out int a);", "out", "int", "in"),
        ("void f(inout String a);", "inout", "String", "in"),
        ("void f(out IBinder a);", "out", "IBinder", "in"),
        ("void f(inout IBinder a);", "inout", "IBinder", "in"),
        ("void f(out @nullable IBinder a);", "out", "IBinder", "in"),
        ("void f(out ICb a);", "out", "interface", "in"),
        ("void f(inout @nullable ICb a);", "inout", "interface", "in"),
        ("void f(out E a);", "out", "enum", "in"),
        ("void f(inout E a);", "inout", "enum", "in"),
        (
            "void f(out ParcelFileDescriptor a);",
            "out",
            "ParcelFileDescriptor",
            "in or inout",
        ),
        (
            "void f(out @nullable ParcelFileDescriptor a);",
            "out",
            "ParcelFileDescriptor",
            "in or inout",
        ),
        ("void f(out Imm a);", "out", "@JavaOnlyImmutable", "in"),
        ("void f(inout Imm a);", "inout", "@JavaOnlyImmutable", "in"),
    ];
    for (method, want_dir, want_kind, want_allowed) in cases {
        let err = generate_method(method).expect_err(method);
        match semantic(&err) {
            SemanticError::InvalidDirection {
                arg,
                direction,
                type_kind,
                allowed,
                help,
                ..
            } => {
                assert_eq!(arg, "a", "{method}");
                assert_eq!(direction, want_dir, "{method}");
                assert_eq!(type_kind, want_kind, "{method}");
                assert_eq!(allowed, want_allowed, "{method}");
                let help = help.as_deref().unwrap_or("");
                assert!(
                    help.contains(&format!("remove '{want_dir}'")),
                    "{method}: {help}"
                );
            }
            other => panic!("{method}: expected InvalidDirection, got: {other}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "invalid direction: 'a' can't be an {want_dir} parameter because {want_kind} \
                 can only be an {want_allowed} parameter"
            ),
            "{method}"
        );
    }
}

#[test]
fn test_out_parcel_file_descriptor_help_offers_inout() {
    let err = generate_method("void f(out ParcelFileDescriptor a);").unwrap_err();
    let help = err.help().map(|h| h.to_string()).unwrap_or_default();
    assert_eq!(help, "remove 'out', or declare it as 'inout'");
}

/// AOSP resolves types before `AidlArgument::CheckValid`, so an unknown type is reported as such.
#[test]
fn test_unknown_argument_type_wins_over_its_direction() {
    for method in [
        "void f(out Foo a);",
        "void f(inout Foo a);",
        "void f(out IFoo.Missing a);",
    ] {
        let err = generate_method(method).expect_err(method);
        assert!(
            matches!(&err, AidlError::Resolution(e)
                if matches!(**e, rsbinder_aidl::error::ResolutionError::UnknownType { .. })),
            "{method}: expected UnknownType, got: {err}"
        );
    }
}

/// AOSP `aidl_unittest.cpp:4367` `RejectsArgumentDirectionNotSpecified`.
#[test]
fn test_omitted_direction_is_rejected_unless_in_is_the_only_one() {
    let cases = [
        ("void f(int[] a);", "array", "in, out, or inout"),
        ("void f(int[3] a);", "array", "in, out, or inout"),
        (
            "void f(@nullable String[] a);",
            "array",
            "in, out, or inout",
        ),
        ("void f(IBinder[] a);", "array", "in, out, or inout"),
        ("void f(ICb[] a);", "array", "in, out, or inout"),
        ("void f(List<String> a);", "List", "in, out, or inout"),
        ("void f(P a);", "parcelable/union", "in, out, or inout"),
        (
            "void f(@nullable P a);",
            "parcelable/union",
            "in, out, or inout",
        ),
        ("void f(G<int> a);", "parcelable/union", "in, out, or inout"),
        ("void f(U a);", "parcelable/union", "in, out, or inout"),
        ("void f(F a);", "parcelable/union", "in, out, or inout"),
        (
            "void f(ParcelFileDescriptor a);",
            "ParcelFileDescriptor",
            "in or inout",
        ),
    ];
    for (method, want_kind, want_allowed) in cases {
        let err = generate_method(method).expect_err(method);
        match semantic(&err) {
            SemanticError::DirectionNotSpecified {
                arg,
                type_kind,
                allowed,
                ..
            } => {
                assert_eq!(arg, "a", "{method}");
                assert_eq!(type_kind, want_kind, "{method}");
                assert_eq!(allowed, want_allowed, "{method}");
            }
            other => panic!("{method}: expected DirectionNotSpecified, got: {other}"),
        }
        assert_eq!(
            err.to_string(),
            format!(
                "missing direction: the direction of 'a' is not specified; {want_kind} can be \
                 an {want_allowed} parameter"
            ),
            "{method}"
        );
    }

    let input = "package test;\ninterface IFoo {\n    void f(int[] a);\n}";
    let err = expect_generation_error(input, "test.aidl");
    let SemanticError::DirectionNotSpecified { span, help, .. } = semantic(&err) else {
        panic!("expected DirectionNotSpecified, got: {err}");
    };
    assert_eq!(&input[span.offset()..span.offset() + span.len()], "int");
    assert_eq!(
        help.as_deref(),
        Some("declare it as 'in', 'out', or 'inout' before the type")
    );
}

/// AOSP `aidl_unittest.cpp:215`, `ITestService.aidl:231`, `ArrayOfInterfaces.aidl:29`.
#[test]
fn test_directions_inside_the_types_aspect_are_accepted() {
    for method in [
        "void f(int a, in int b, String c, in String d);",
        "void f(IBinder a, in @nullable IBinder b, ICb c, in ICb d, E e, in E g);",
        "void f(in ParcelFileDescriptor a, inout ParcelFileDescriptor b);",
        "void f(inout @nullable ParcelFileDescriptor a);",
        "void f(in Imm a, Imm b);",
        "void f(in int[] a, out int[] b, inout int[] c);",
        "void f(out int[3] a, inout int[2][3] b);",
        "void f(out IBinder[] a, inout @nullable IBinder[] b);",
        "void f(out ICb[] a, inout @nullable ICb[] b);",
        "void f(out ParcelFileDescriptor[] a);",
        "void f(out E[] a);",
        "void f(in List<String> a, out List<String> b, inout List<IBinder> c);",
        "void f(in P a, out P b, inout @nullable P c);",
        "void f(out G<int> a, inout U b, out F c);",
        "IBinder f(in IBinder a);",
    ] {
        if let Err(e) = generate_method(method) {
            panic!("{method}: expected success, got: {e:?}");
        }
    }
}

/// AOSP `aidl_language.cpp:1236`: a oneway method refuses `out`/`inout` for every type.
#[test]
fn test_oneway_method_refuses_out_even_for_arrays() {
    for method in ["oneway void f(out int[] a);", "oneway void f(inout P a);"] {
        let input = format!("package test;\ninterface IFoo {{\n{DIRECTION_DECLS}    {method}\n}}");
        let ctx = SourceContext::new("test.aidl", &input);
        let err = match parse_document(&ctx) {
            Err(e) => e,
            Ok(doc) => Generator::new(false, false)
                .document(&doc)
                .map(|_| ())
                .expect_err(method),
        };
        assert!(
            format!("{err:?}").contains("oneway method 'f' cannot have"),
            "{method}: {err:?}"
        );
    }
}

// ── @Backing(type=...) validation (AOSP allowlist: byte/int/long) ────────────

/// Asserts an InvalidBackingType diagnostic labelled exactly at `expected_annotation`.
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
fn test_list_backing_type_is_an_invalid_backing_type_error() {
    // Backing validation runs before TypeGenerator, so List is InvalidBackingType (AOSP).
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
    // The implicit byte backing passes @Backing validation.
    let input = "package foo;\nenum MyEnum { V1 = 1 }";
    let ctx = SourceContext::new("test.aidl", input);
    let doc = parse_document(&ctx).expect("parse");
    let gen = Generator::new(false, false);
    let (_, out) = gen.document(&doc).expect("generate");
    assert!(out.contains("r#MyEnum : [i8; 1]"), "{out}");
}

/// AOSP schema `{"type", kStringType, required}`: no `type` is an error, not a byte default.
#[test]
fn test_backing_without_type_is_rejected() {
    for input in [
        "package foo;\n@Backing(\"long\")\nenum MyEnum { V1 = 1 }",
        "package foo;\n@Backing(typ=\"long\")\nenum MyEnum { V1 = 1 }",
        "package foo;\n@Backing\nenum MyEnum { V1 = 1 }",
    ] {
        let ctx = SourceContext::new("test.aidl", input);
        let doc = parse_document(&ctx).expect("parse");
        assert!(
            Generator::new(false, false).document(&doc).is_err(),
            "{input}"
        );
    }
}

/// AOSP `AidlTypenames` "redefinition"; accepting it would emit `pub mod A` twice (E0428).
#[test]
fn test_same_type_in_two_files_is_a_redefinition() {
    let tmp = scratch_dir("redefinition");
    let first = tmp.join("first");
    let second = tmp.join("second");
    for dir in [&first, &second] {
        std::fs::create_dir_all(dir.join("test")).unwrap();
        std::fs::write(
            dir.join("test/A.aidl"),
            "package test;\nparcelable A { int x; }",
        )
        .unwrap();
    }

    let err = rsbinder_aidl::Builder::new()
        .source(first.join("test/A.aidl"))
        .source(second.join("test/A.aidl"))
        .output(&tmp)
        .generate()
        .expect_err("the second declaration of test.A must be refused");
    let message = err.to_string();
    assert!(
        message.contains("'test.A'") && message.contains("first") && message.contains("second"),
        "got: {message}"
    );
}

/// AOSP `AddDocument` recurses into nested types: a nested and a top-level `a.b.c` collide.
#[test]
fn test_nested_and_top_level_type_of_one_name_is_a_redefinition() {
    for (name, nested, top_level, qualified) in [
        (
            "redefinition_nested",
            (
                "a/b.aidl",
                "package a;\nparcelable b { parcelable c { int x; } }",
            ),
            ("a/b/c.aidl", "package a.b;\nparcelable c { int y; }"),
            "'a.b.c'",
        ),
        (
            "redefinition_union_tag",
            ("a/U.aidl", "package a;\nunion U { int x; }"),
            ("a/U/Tag.aidl", "package a.U;\nparcelable Tag { int y; }"),
            "'a.U.Tag'",
        ),
    ] {
        let tmp = scratch_dir(name);
        let mut builder = rsbinder_aidl::Builder::new().output(&tmp);
        for (dir, (path, text)) in [("first", nested), ("second", top_level)] {
            let file = tmp.join(dir).join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, text).unwrap();
            builder = builder.source(file);
        }
        let err = builder
            .generate()
            .expect_err("one qualified name declared twice must be refused");
        let message = err.to_string();
        assert!(
            message.contains(&format!("type {qualified} is defined in both")),
            "got: {message}"
        );
    }
}

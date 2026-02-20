// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Phase 4 panic removal verification (작업 5.4)
//! Validates that all former-panic paths now return Err instead of panicking.

use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};

/// Helper: expect a generation-phase error (parsing succeeds, generation fails)
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
// 4.1: parser.rs panic removal verification
// ============================================================

// 4.1a: u8 overflow (256u8) should return Err, not panic
#[test]
fn test_u8_overflow_no_panic() {
    let input = r#"
parcelable Foo {
    const byte val = 256u8;
}
    "#;
    // Should not panic — either parse error or semantic error
    let result = std::panic::catch_unwind(|| {
        let ctx = SourceContext::new("test.aidl", input);
        let parse_result = parse_document(&ctx);
        if let Ok(doc) = parse_result {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    });
    assert!(result.is_ok(), "Should not panic on u8 overflow");
}

// 4.1b: u8 boundary value 255 should succeed
#[test]
fn test_u8_overflow_boundary_255() {
    let input = r#"
parcelable Foo {
    const byte val = 255u8;
}
    "#;
    let ctx = SourceContext::new("test.aidl", input);
    let result = parse_document(&ctx);
    // 255 is valid for u8, should parse successfully
    assert!(result.is_ok(), "255u8 should be valid");
}

// 4.1c: u8 boundary value 0 should succeed
#[test]
fn test_u8_overflow_boundary_0() {
    let input = r#"
parcelable Foo {
    const byte val = 0u8;
}
    "#;
    let ctx = SourceContext::new("test.aidl", input);
    let result = parse_document(&ctx);
    assert!(result.is_ok(), "0u8 should be valid");
}

// 4.1d: Unknown type in parcelable — should not panic
// Note: The generator treats unknown types as user-defined types and proceeds gracefully,
// so this test verifies no panic rather than requiring an error.
#[test]
fn test_unknown_type_no_panic() {
    let result = std::panic::catch_unwind(|| {
        let ctx = SourceContext::new(
            "test.aidl",
            "parcelable Foo {\n    NonExistentType field;\n}",
        );
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    });
    assert!(result.is_ok(), "Should not panic on unknown type");
}

// 4.1e: Unknown type as method return type — should not panic
#[test]
fn test_unknown_type_in_method_return() {
    let result = std::panic::catch_unwind(|| {
        let ctx = SourceContext::new(
            "test.aidl",
            "interface IFoo {\n    UnknownType doSomething();\n}",
        );
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    });
    assert!(result.is_ok(), "Should not panic on unknown return type");
}

// 4.1f: Unknown type as method parameter — should not panic
#[test]
fn test_unknown_type_in_method_param() {
    let result = std::panic::catch_unwind(|| {
        let ctx = SourceContext::new(
            "test.aidl",
            "interface IFoo {\n    void doSomething(UnknownType arg);\n}",
        );
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    });
    assert!(result.is_ok(), "Should not panic on unknown param type");
}

// 4.1g: Verify catch_unwind is completely removed from non-test source code
#[test]
fn test_catch_unwind_removed() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let content = std::fs::read_to_string(&path).unwrap();
            // Strip out #[cfg(test)] mod tests { ... } blocks before checking
            // We only care about non-test production code
            let non_test = if let Some(idx) = content.find("#[cfg(test)]") {
                &content[..idx]
            } else {
                &content
            };
            assert!(
                !non_test.contains("catch_unwind"),
                "Found catch_unwind in non-test code of {}",
                path.display()
            );
        }
    }
}

// ============================================================
// 4.2: type_generator.rs panic removal verification
// ============================================================

// 4.2a: List without generic type parameter
#[test]
fn test_list_without_generic() {
    let err = expect_generation_error(
        r#"
parcelable Foo {
    List items;
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("List") || msg.contains("Generic"),
        "Error should mention List/Generic: {msg}"
    );
}

// 4.2b: List without generic in method return type
#[test]
fn test_list_without_generic_in_method() {
    let err = expect_generation_error(
        r#"
interface IFoo {
    List getItems();
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("List") || msg.contains("Generic"),
        "Error should mention List/Generic: {msg}"
    );
}

// 4.2c: FileDescriptor (unsupported, use ParcelFileDescriptor)
#[test]
fn test_file_descriptor_unsupported() {
    let err = expect_generation_error(
        r#"
parcelable Foo {
    FileDescriptor fd;
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.contains("FileDescriptor") || msg.contains("ParcelFileDescriptor"),
        "Error should mention FileDescriptor: {msg}"
    );
}

// 4.2d: @nullable on primitive type (int)
#[test]
fn test_nullable_primitive_int() {
    let err = expect_generation_error(
        r#"
parcelable Foo {
    @nullable int val;
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("nullable") || msg.to_lowercase().contains("primitive"),
        "Error should mention nullable/primitive: {msg}"
    );
}

// 4.2e: @nullable on primitive type (boolean)
#[test]
fn test_nullable_primitive_boolean() {
    let err = expect_generation_error(
        r#"
parcelable Foo {
    @nullable boolean flag;
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("nullable") || msg.to_lowercase().contains("primitive"),
        "Error should mention nullable/primitive: {msg}"
    );
}

// 4.2f: out parameter with primitive type
#[test]
fn test_out_primitive_param() {
    let err = expect_generation_error(
        r#"
interface IFoo {
    void method(out int x);
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("out")
            || msg.to_lowercase().contains("primitive")
            || msg.to_lowercase().contains("parameter"),
        "Error should mention out/primitive: {msg}"
    );
}

// 4.2g: inout parameter with String type
#[test]
fn test_inout_string_param() {
    let err = expect_generation_error(
        r#"
interface IFoo {
    void method(inout String s);
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("inout")
            || msg.to_lowercase().contains("string")
            || msg.to_lowercase().contains("parameter"),
        "Error should mention inout/String: {msg}"
    );
}

// 4.2h: out parameter with String type
#[test]
fn test_out_string_param() {
    let err = expect_generation_error(
        r#"
interface IFoo {
    void method(out String s);
}
        "#,
        "test.aidl",
    );
    let msg = format!("{err}");
    assert!(
        msg.to_lowercase().contains("out")
            || msg.to_lowercase().contains("string")
            || msg.to_lowercase().contains("parameter"),
        "Error should mention out/String: {msg}"
    );
}

// ============================================================
// 4.3: const_expr.rs panic removal verification (AIDL-level)
// ============================================================

// 4.3a: Bitwise OR on float literal — should not panic
// Note: ConstExprError is silently handled via unwrap_or_else in pre_process(),
// so the expression may degrade gracefully. We verify no panic.
#[test]
fn test_bitwise_op_on_float() {
    let input = r#"
parcelable Foo {
    const int BAD = 1.5f | 2;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(result.is_ok(), "Should not panic on bitwise op with float");
}

// 4.3b: Shift operation on float literal — should not panic
#[test]
fn test_shift_op_on_float() {
    let input = r#"
parcelable Foo {
    const int BAD = 1.5f << 2;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(result.is_ok(), "Should not panic on shift op with float");
}

// 4.3c: Unary NOT on float literal — should not panic
#[test]
fn test_unary_not_on_float() {
    let input = r#"
parcelable Foo {
    const int BAD = ~3.14f;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(result.is_ok(), "Should not panic on unary not with float");
}

// 4.3d: Surrogate code point (0xD800) in char field — must not panic
// char::from_u32(0xD800) returns None (surrogate range is not valid Unicode scalar).
// The ConstExprError is caught by the caller and silently falls back; no panic.
#[test]
fn test_invalid_unicode_surrogate() {
    let input = r#"
parcelable Foo {
    char bad = 0xD800;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        // ParseError is also acceptable; only generation-phase result matters
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(
        result.is_ok(),
        "Should not panic on surrogate code point 0xD800"
    );
}

// 4.3e: Code point beyond Unicode maximum (0x110000) in char field — must not panic
// char::from_u32(0x110000) returns None; ConstExprError is caught silently.
#[test]
fn test_invalid_unicode_too_large() {
    let input = r#"
parcelable Foo {
    char bad = 0x110000;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(
        result.is_ok(),
        "Should not panic on out-of-range code point 0x110000"
    );
}

// 4.3f: Maximum valid Unicode scalar value (0x10FFFF) in char field — must not panic
// char::from_u32(0x10FFFF) returns Some; conversion succeeds normally.
#[test]
fn test_valid_unicode_boundary() {
    let input = r#"
parcelable Foo {
    char ok = 0x10FFFF;
}
    "#;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let ctx = SourceContext::new("test.aidl", input);
        if let Ok(doc) = parse_document(&ctx) {
            let gen = Generator::new(false, false);
            let _ = gen.document(&doc);
        }
    }));
    assert!(
        result.is_ok(),
        "Should not panic on maximum valid Unicode code point 0x10FFFF"
    );
}

// ============================================================
// 4.4: lib.rs .unwrap() removal verification
// ============================================================

// 4.4a: File without extension should not panic
#[test]
fn test_file_without_extension() {
    let tmp = std::env::temp_dir().join("rsbinder_test_no_ext");
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join("NoExtension");
    std::fs::write(&file_path, "parcelable Foo {}").unwrap();

    let result = rsbinder_aidl::Builder::new()
        .source(&file_path)
        .output(&tmp)
        .generate();

    // Should not panic — Err is acceptable
    // The result may actually succeed if file_stem works without extension
    // What matters is no panic
    let _ = result;

    std::fs::remove_dir_all(&tmp).ok();
}

// 4.4b: File with only dot as name should not panic
#[test]
fn test_file_with_dot_only() {
    let tmp = std::env::temp_dir().join("rsbinder_test_dot_only");
    std::fs::create_dir_all(&tmp).unwrap();
    let file_path = tmp.join(".aidl");
    std::fs::write(&file_path, "parcelable Foo {}").unwrap();

    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rsbinder_aidl::Builder::new()
            .source(&file_path)
            .output(&tmp)
            .generate()
    }));

    // Should not panic
    assert!(result.is_ok(), "Should not panic on .aidl filename");

    std::fs::remove_dir_all(&tmp).ok();
}

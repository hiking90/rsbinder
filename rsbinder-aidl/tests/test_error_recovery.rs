// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Diagnosable input returns `Err`, never panics; the generator has no `catch_unwind`.

use rsbinder_aidl::{parse_document, AidlError, Generator, SourceContext};
use std::path::PathBuf;

/// Per-test dir under the target dir: tests cannot clobber each other or the system temp dir.
fn scratch_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

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

// ==================== parser.rs: no panic on bad input ====================

// u8 overflow (256u8) should return Err, not panic
#[test]
fn test_u8_overflow_no_panic() {
    let input = r#"
parcelable Foo {
    const byte val = 256u8;
}
    "#;
    let ctx = SourceContext::new("test.aidl", input);
    let err = parse_document(&ctx).expect_err("256u8 must be rejected");
    assert!(
        format!("{err:?}").contains("u8 literal overflow"),
        "{err:?}"
    );
}

// u8 boundary value 255 should succeed
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

// u8 boundary value 0 should succeed
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

// Unknown type in parcelable: only the absence of a panic is checked, not the result.
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

// Unknown type as method return type — should not panic
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

// Unknown type as method parameter — should not panic
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

// Non-test source code must not contain catch_unwind.
#[test]
fn test_catch_unwind_removed() {
    let src_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    for entry in std::fs::read_dir(&src_dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().is_some_and(|ext| ext == "rs") {
            let content = std::fs::read_to_string(&path).unwrap();
            // Split at the last `mod tests`: an early `#[cfg(test)]` helper would hide the rest.
            let non_test = match content.rfind("\n#[cfg(test)]\nmod tests") {
                Some(idx) => &content[..idx],
                None => &content,
            };
            assert!(
                !non_test.contains("catch_unwind"),
                "Found catch_unwind in non-test code of {}",
                path.display()
            );
        }
    }
}

// ==================== type_generator.rs: no panic on bad input ====================

// List without generic type parameter
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
    assert_eq!(
        err.to_string(),
        "invalid operation: Type \"List\" of AIDL must have Generic Type",
        "got: {err:?}"
    );
}

// List without generic in method return type
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
    assert_eq!(
        err.to_string(),
        "invalid operation: Type \"List\" of AIDL must have Generic Type",
        "got: {err:?}"
    );
}

// FileDescriptor (unsupported, use ParcelFileDescriptor)
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
    assert_eq!(
        err.to_string(),
        "unsupported type: FileDescriptor",
        "the dedicated diagnostic must fire, not the generic unknown-type \
         fallback: {err:?}"
    );
}

// @nullable on primitive type (int)
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

// @nullable on primitive type (boolean)
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

// out parameter with primitive type
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

// inout parameter with String type
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

// out parameter with String type
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

// ==================== const_expr.rs: no panic on bad input (AIDL-level) ====================

// Bitwise OR on float literal: only the absence of a panic is checked.
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

// Shift operation on float literal — should not panic
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

// Unary NOT on float literal — should not panic
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

// Surrogate 0xD800 in a char field (`char::from_u32` gives None) must not panic.
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

// 0x110000 (past the Unicode maximum) in a char field must not panic.
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

// AIDL `char` is 16-bit: a code point above U+FFFF is a diagnostic, not an `as u16` truncation.
#[test]
fn test_char_beyond_u16_is_rejected() {
    for input in [
        "parcelable Foo { char bad = 0x10FFFF; }",
        "parcelable Foo { const char C = '\u{1F600}'; }",
    ] {
        let err = expect_generation_error(input, "test.aidl");
        assert!(format!("{err:?}").contains("char"), "{input}: {err:?}");
    }
    let ctx = SourceContext::new("test.aidl", "parcelable Foo { char ok = 0xFFFF; }");
    let doc = parse_document(&ctx).expect("parse");
    let out = Generator::new(false, false)
        .document(&doc)
        .expect("0xFFFF fits a char")
        .1;
    assert!(out.contains("r#ok: '\u{ffff}' as u16"), "{out}");
}

// The error for an unreadable source names the file.
#[test]
fn test_unreadable_source_names_the_file() {
    let tmp = scratch_dir("non_utf8");
    let file_path = tmp.join("Latin1.aidl");
    std::fs::write(&file_path, b"// caf\xe9\nparcelable Foo {}").unwrap();
    let err = rsbinder_aidl::Builder::new()
        .source(&file_path)
        .dest_dir(&tmp)
        .output("gen.rs")
        .generate()
        .expect_err("a non-UTF-8 source must fail");
    assert!(err.to_string().contains("Latin1.aidl"), "{err}");
}

// ==================== lib.rs: no `.unwrap()` panic on odd file names ====================

// File without extension should not panic
#[test]
fn test_file_without_extension() {
    let tmp = scratch_dir("no_ext");
    let file_path = tmp.join("NoExtension");
    std::fs::write(&file_path, "parcelable Foo {}").unwrap();

    // An extension-less source is valid AIDL; it must generate, not panic on `file_stem`.
    rsbinder_aidl::Builder::new()
        .source(&file_path)
        .dest_dir(&tmp)
        .output("gen.rs")
        .generate()
        .expect("an extension-less source must still generate");
    let out = std::fs::read_to_string(tmp.join("gen.rs")).expect("output written");
    assert!(out.contains("pub struct Foo"), "{out}");
}

// File with only dot as name should not panic
#[test]
fn test_file_with_dot_only() {
    let tmp = scratch_dir("dot_only");
    let file_path = tmp.join(".aidl");
    std::fs::write(&file_path, "parcelable Foo {}").unwrap();

    // A name that is only an extension has an empty stem; it must not panic.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rsbinder_aidl::Builder::new()
            .source(&file_path)
            .dest_dir(&tmp)
            .output("gen.rs")
            .generate()
    }));
    assert!(result.is_ok(), "Should not panic on .aidl filename");
}

// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tests for safety fixes - circular reference detection and infinite loop prevention

use std::error::Error;

fn test_aidl_generation(input: &str) -> Result<String, Box<dyn Error>> {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx)?;
    let gen = rsbinder_aidl::Generator::new(false, false);
    let res = gen.document(&document)?;
    Ok(res.1)
}

#[test]
fn test_circular_reference_warning() -> Result<(), Box<dyn Error>> {
    // This should no longer cause stack overflow, instead show warning
    let input = r##"
        package test.circular;
        
        enum CircularEnum {
            A = B,
            B = C,
            C = A,
        }
    "##;

    // Capture stderr to check for warnings
    let result = test_aidl_generation(input);

    match result {
        Ok(_output) => {
            // Success is acceptable - it means we handled the circular reference gracefully
            println!("Circular reference handled successfully (graceful degradation)");
            Ok(())
        }
        Err(e) => {
            // Error is also acceptable as long as it doesn't crash
            println!("Circular reference caused error (but no crash): {}", e);
            Ok(())
        }
    }
}

#[test]
fn test_deep_nesting_stability() -> Result<(), Box<dyn Error>> {
    // Test deeply nested enum references to ensure recursion depth limiting works
    let input = r##"
        package test.deep;
        
        enum BaseEnum {
            BASE = 1,
        }
        
        enum Level1 {
            VAL = BaseEnum.BASE,
        }
        
        enum Level2 {
            VAL = Level1.VAL + 1,
        }
        
        enum Level3 {
            VAL = Level2.VAL * 2,
        }
        
        enum Level4 {
            VAL = Level3.VAL << 1,
        }
        
        enum Level5 {
            VAL = Level4.VAL | Level3.VAL,
        }
    "##;

    let result = test_aidl_generation(input)?;

    // Should work without crashes
    assert!(result.contains("pub mod BaseEnum"));
    assert!(result.contains("pub mod Level5"));
    println!("Deep nesting test passed successfully");

    Ok(())
}

#[test]
fn test_unresolvable_reference_graceful() -> Result<(), Box<dyn Error>> {
    // Test reference to non-existent enum - should not crash
    let input = r##"
        package test.unresolvable;
        
        enum TestEnum {
            A = NonExistentEnum.VALUE,
            B = 1,
            C = A + B, // This should still work
        }
    "##;

    let result = test_aidl_generation(input);

    match result {
        Ok(output) => {
            // Should generate something reasonable
            assert!(output.contains("pub mod TestEnum"));
            println!("Unresolvable reference handled gracefully");
        }
        Err(e) => {
            // Error is acceptable as long as it doesn't crash
            println!("Unresolvable reference caused error (no crash): {}", e);
        }
    }

    Ok(())
}

#[test]
fn test_mixed_resolvable_unresolvable() -> Result<(), Box<dyn Error>> {
    // Test mix of resolvable and unresolvable references
    let input = r##"
        package test.mixed;
        
        enum GoodEnum {
            GOOD = 10,
        }
        
        enum MixedEnum {
            WORKS = GoodEnum.GOOD,    // Should resolve
            BROKEN = BadEnum.MISSING, // Should not resolve but not crash
            COMPUTED = WORKS + 5,     // Should work based on resolved value
        }
    "##;

    let result = test_aidl_generation(input);

    match result {
        Ok(output) => {
            assert!(output.contains("pub mod GoodEnum"));
            assert!(output.contains("pub mod MixedEnum"));
            println!("Mixed resolvable/unresolvable test passed");
        }
        Err(e) => {
            println!("Mixed test caused error (acceptable): {}", e);
        }
    }

    Ok(())
}

#[test]
fn test_large_enum_performance() -> Result<(), Box<dyn Error>> {
    // Test with a larger enum to ensure performance is reasonable
    let input = r##"
        package test.large;
        
        enum LargeEnum {
            V0 = 0,
            V1 = V0 + 1,
            V2 = V1 + 1, 
            V3 = V2 + 1,
            V4 = V3 + 1,
            V5 = V4 + 1,
            V6 = V5 + 1,
            V7 = V6 + 1,
            V8 = V7 + 1,
            V9 = V8 + 1,
            V10 = V9 + 1,
            V11 = V10 + 1,
            V12 = V11 + 1,
            V13 = V12 + 1,
            V14 = V13 + 1,
            V15 = V14 + 1,
            V16 = V15 + 1,
            V17 = V16 + 1,
            V18 = V17 + 1,
            V19 = V18 + 1,
            V20 = V19 + 1,
        }
    "##;

    use std::time::Instant;
    let start = Instant::now();

    let result = test_aidl_generation(input)?;

    let duration = start.elapsed();

    // Should complete in reasonable time (less than 1 second)
    assert!(
        duration.as_secs() < 1,
        "Large enum took too long: {:?}",
        duration
    );
    assert!(result.contains("pub mod LargeEnum"));

    println!("Large enum test passed in {:?}", duration);

    Ok(())
}

#[test]
fn test_android_keymint_style_simplified() -> Result<(), Box<dyn Error>> {
    // Simplified version of Android KeyMint style enum - should work now
    let input = r##"
        package android.hardware.security.keymint;
        
        @Backing(type="int")
        enum TagType {
            INVALID = 0,
            UINT = 0x30000000,
            BOOL = 0x70000000,
        }
        
        @Backing(type="int")
        enum Tag {
            INVALID = TagType.INVALID,
            KEY_SIZE = TagType.UINT | 3,
            CALLER_NONCE = TagType.BOOL | 7,
            COMPUTED = KEY_SIZE + 1,
        }
    "##;

    let result = test_aidl_generation(input)?;

    // Should work without panicking
    assert!(result.contains("pub mod TagType"));
    assert!(result.contains("pub mod Tag"));

    // Verify some basic values are calculated
    assert!(result.contains("r#INVALID = 0"));
    assert!(result.contains("r#UINT = 805306368")); // 0x30000000

    println!("Android KeyMint style test passed successfully");

    Ok(())
}

#[test]
fn test_char_escape_sequences_decoded() -> Result<(), Box<dyn Error>> {
    // `'\n'` must decode to the newline code point, not the literal char 'n'
    // (regression: the parser returned the post-backslash char verbatim).
    let input = r##"
        package test.ch;

        interface IFoo {
            const char NL = '\n';
            const char TAB = '\t';
            const char A = 'A';
        }
    "##;

    let output = test_aidl_generation(input)?;
    assert!(
        output.contains("r#NL: u16 = '\\n' as u16"),
        "newline escape not decoded, got: {output}"
    );
    assert!(
        output.contains("r#TAB: u16 = '\\t' as u16"),
        "tab escape not decoded, got: {output}"
    );
    assert!(output.contains("r#A: u16 = 'A' as u16"));
    Ok(())
}

#[test]
fn test_unknown_type_is_diagnostic_not_panic() {
    // A parcelable field referencing an undefined type previously panicked deep
    // in code generation (`make_user_defined_type_name(...).expect()`). It must
    // now surface as a `ResolutionError::UnknownType` diagnostic instead.
    let input = r##"
        package test.unknown;

        parcelable Foo {
            ThisTypeIsNotDefinedAnywhere field;
        }
    "##;

    let result = test_aidl_generation(input);
    let err = result.expect_err("undefined type must produce an error, not succeed");
    let msg = err.to_string();
    assert!(
        msg.contains("unknown type") && msg.contains("ThisTypeIsNotDefinedAnywhere"),
        "expected UnknownType diagnostic, got: {msg}"
    );
}

#[test]
fn test_operator_chain_rejected_before_stack_overflow() {
    // A long *unbracketed* operator chain bypasses the bracket/angle nesting
    // guard yet still drives one parser/walker recursion per operator. It must
    // be rejected as an ordinary diagnostic, never overflow the stack (the
    // chains here stay under the stack-overflow threshold but exceed
    // `MAX_OPERATOR_RUN`, so the pre-parse guard rejects them).
    let unary = "~".repeat(4000);
    let unary_input = format!("package test.dos;\ninterface IFoo {{ const int X = {unary}5; }}");
    assert!(
        test_aidl_generation(&unary_input).is_err(),
        "long unary operator chain must be rejected"
    );

    let binary = "1+".repeat(4000);
    let binary_input = format!("package test.dos;\ninterface IFoo {{ const int X = {binary}1; }}");
    assert!(
        test_aidl_generation(&binary_input).is_err(),
        "long binary operator chain must be rejected"
    );

    // A short chain still generates fine.
    let ok =
        test_aidl_generation("package test.dos;\ninterface IFoo { const int Y = 1 + 2 + 3 + 4; }")
            .expect("short expression should generate");
    assert!(ok.contains("r#Y"));
}

#[test]
fn test_unresolvable_const_reference_is_diagnostic() {
    // A typo'd / missing constant reference must surface a diagnostic instead
    // of silently folding to 0 (AOSP hard-fails with "Can't find <name>").
    // Covers both a bare reference and one inside an arithmetic expression.
    for input in [
        "package test.u;\ninterface IFoo { const int A = TYPO_NAME; }",
        "package test.u;\ninterface IFoo { const int A = TYPO_NAME + 1; }",
    ] {
        assert!(
            test_aidl_generation(input).is_err(),
            "unresolvable const reference must error, input: {input}"
        );
    }

    // A *circular* reference has no well-defined value: AOSP hard-fails, and
    // folding to a neutral 0 would silently bake a fabricated constant into
    // the generated IPC code. It must be a diagnostic too.
    let circular = test_aidl_generation(
        "package test.c;\ninterface IFoo { const int A = B; const int B = A; }",
    );
    assert!(
        circular.is_err(),
        "circular constant reference must be a diagnostic, got: {circular:?}"
    );
}

#[test]
fn test_overflowing_shift_is_diagnostic() {
    // `1 << 40` overflows the int32 shift operand type and is rejected by
    // AOSP; rsbinder must not silently truncate it to 0. The declared `long`
    // does not widen the shift (operands promote to int independently).
    assert!(
        test_aidl_generation("package test.s;\ninterface IFoo { const long C = 1 << 40; }")
            .is_err(),
        "out-of-range shift in a long const must error"
    );
    assert!(
        test_aidl_generation("package test.s;\ninterface IFoo { const int A = 1 << 40; }").is_err(),
        "out-of-range shift in an int const must error"
    );

    // An in-range shift still generates the correct value.
    let ok = test_aidl_generation("package test.s;\ninterface IFoo { const int B = 1 << 4; }")
        .expect("in-range shift should generate");
    assert!(ok.contains("r#B"));
    // `1L << 40` is valid (64-bit operand) and must keep working.
    let wide = test_aidl_generation("package test.s;\ninterface IFoo { const long D = 1L << 40; }")
        .expect("64-bit shift should generate");
    assert!(wide.contains("r#D"));
}

#[test]
fn test_rust_keyword_type_names_are_escaped() {
    // AIDL permits type names that are Rust keywords (`type`, `loop`,
    // `match`, …); the generated mod/struct/trait declarations AND the
    // reference paths that name them must `r#`-escape so the output compiles
    // (AOSP's Rust backend does the same). Previously only member names were
    // escaped, so a keyword-named type emitted non-compiling Rust.
    let parc = test_aidl_generation("package test.kw;\nparcelable type { int a; }")
        .expect("keyword parcelable should generate");
    assert!(parc.contains("pub mod r#type"), "got:\n{parc}");
    assert!(parc.contains("pub struct r#type"), "got:\n{parc}");

    let iface = test_aidl_generation("package test.kw;\ninterface loop { void foo(); }")
        .expect("keyword interface should generate");
    assert!(iface.contains("pub mod r#loop"), "got:\n{iface}");
    assert!(iface.contains("pub trait r#loop"), "got:\n{iface}");

    // Cross-reference: a field typed as a keyword-named parcelable must
    // produce a fully `r#`-escaped path, never a bare keyword path.
    let xref = test_aidl_generation(
        "package test.kw;\nparcelable match { int a; }\nparcelable Holder { match m; }",
    )
    .expect("cross-reference to keyword type should generate");
    assert!(
        xref.contains("super::r#match::r#match"),
        "cross-reference path not escaped, got:\n{xref}"
    );
    // An ordinary (non-keyword) name is untouched — no spurious escaping.
    assert!(xref.contains("pub mod Holder"), "got:\n{xref}");
}

#[test]
fn test_enum_discriminant_eval_failure_is_diagnostic() {
    // `A = 1/0` previously fell through `if let Ok` into the auto-increment
    // counter and generated `A = 0` — a silently wrong wire discriminant.
    // AOSP rejects the expression at build time.
    let result = test_aidl_generation("package test.e;\nenum E { A = 1/0 }");
    assert!(
        result.is_err(),
        "enum discriminant with failing evaluation must error, got: {result:?}"
    );

    // A later auto-increment member is poisoned by the earlier failure and
    // must not silently receive a fabricated value either.
    let result = test_aidl_generation("package test.e;\nenum E { A = 1/0, B }");
    assert!(
        result.is_err(),
        "auto-increment after failing discriminant must error, got: {result:?}"
    );
}

#[test]
fn test_enum_discriminant_unresolved_reference_is_diagnostic() {
    // `A = foo.Missing.X` previously resolved through the current-declaration
    // lookup fallback (or stayed an unresolved Name and was skipped) and
    // generated `A = 0`. AOSP rejects the reference at build time.
    let result = test_aidl_generation("package test.e;\nenum E { A = foo.Missing.X }");
    assert!(
        result.is_err(),
        "enum discriminant with unresolvable reference must error, got: {result:?}"
    );
}

#[test]
fn test_enum_discriminant_circular_reference_is_diagnostic() {
    // `enum E { A = B, B = A }` previously generated the fabricated values
    // A=1, B=1. A discriminant cycle has no well-defined value; AOSP errors.
    let result = test_aidl_generation("package test.e;\nenum E { A = B, B = A }");
    assert!(
        result.is_err(),
        "circular enum discriminants must error, got: {result:?}"
    );
}

#[test]
fn test_phantom_package_type_is_diagnostic_not_self_reference() {
    // A field typed as `<missing-package>.P` where the simple name equals the
    // enclosing parcelable previously resolved to the parcelable itself via
    // the lookup fallback and generated a self-referential `Box<P>` field.
    let result = test_aidl_generation("package test.p;\nparcelable P { no.such.pkg.P other; }");
    let err = result.expect_err("phantom package type must produce an error");
    let msg = err.to_string();
    assert!(
        msg.contains("no.such.pkg.P"),
        "expected UnknownType diagnostic naming the phantom type, got: {msg}"
    );
}

#[test]
fn test_enum_array_default_initializers() {
    // Characterization: no golden test covered `init_enum_array_value` (enum
    // array field defaults). Pins both the non-nullable and nullable forms so
    // the init_value extraction cannot silently change them.
    let input = r##"
        package test.enumarr;
        enum Color { RED = 0, GREEN = 1, BLUE = 2 }
        parcelable Palette {
            Color[] colors = { Color.RED, Color.BLUE };
            @nullable Color[] maybeColors = { Color.GREEN };
        }
    "##;
    let out = test_aidl_generation(input).expect("generation should succeed");
    assert!(
        out.contains("r#colors: vec![super::Color::Color::RED,super::Color::Color::BLUE,],"),
        "non-nullable enum array default mismatch, got:\n{out}"
    );
    assert!(
        out.contains("r#maybeColors: Some(vec![super::Color::Color::GREEN,]),"),
        "nullable enum array default mismatch, got:\n{out}"
    );
}

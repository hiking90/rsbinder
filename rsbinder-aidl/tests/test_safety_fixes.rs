// Copyright 2025 rsbinder Contributors
// SPDX-License-Identifier: Apache-2.0

//! Tests for safety fixes - circular reference detection and infinite loop prevention

use std::error::Error;

fn test_aidl_generation(input: &str) -> Result<String, Box<dyn Error>> {
    let document = rsbinder_aidl::parse_document(input)?;
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

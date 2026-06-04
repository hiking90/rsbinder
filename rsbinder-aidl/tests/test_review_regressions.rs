// SPDX-License-Identifier: Apache-2.0
//
// Regression tests for codegen defects found in the full-source review.
// Each case used to abort the AIDL compiler (hard `panic!` / stack
// overflow / debug overflow) on parseable input, violating the project's
// "no panic on user input" invariant. They must now surface as recoverable
// errors (or compute without panicking).

/// Returns `true` only when BOTH parsing and code generation succeed.
fn generate_ok(input: &str) -> bool {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let Ok(document) = rsbinder_aidl::parse_document(&ctx) else {
        return false;
    };
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).is_ok()
}

/// M-6: a `List<T[]>` (list-of-array) is grammar-valid but used to hit the
/// unconditional `panic!("type_decl() can't process Array Type.")`. It must
/// now be rejected with a diagnostic, while a plain `List<T>` still works.
#[test]
fn list_of_array_is_rejected_not_panicked() {
    for src in [
        "parcelable P { List<int[]> field; }",
        "interface I { void m(in List<String[]> a); }",
        "interface I2 { List<int[]> r(); }",
    ] {
        assert!(
            !generate_ok(src),
            "expected list-of-array to error: {src:?}"
        );
    }
    assert!(
        generate_ok("parcelable P { List<int> field; }"),
        "legitimate List<int> must still generate"
    );
}

/// M-7: mutually-referential constants used to recurse until the stack
/// overflowed (the cycle guard was abandoned when evaluation crossed a
/// binary operator). Generation must now terminate; non-cyclic chains that
/// also cross operators must still resolve.
#[test]
fn cyclic_constants_do_not_overflow() {
    // Reaching the end of this call without aborting is the assertion.
    let _ = generate_ok("interface ICycle { const int A = B + 1; const int B = A + 1; }");

    assert!(
        generate_ok("interface IOk { const int A = 1; const int B = A + 1; const int C = A + B; }"),
        "non-cyclic constant chain must still resolve"
    );
}

/// L-3: an `i64::MAX` enumerator followed by an auto-increment member used
/// to panic on debug builds (`enum_val += 1` overflow). It must now wrap
/// (AOSP C++ semantics) without aborting.
#[test]
fn enum_autoincrement_overflow_does_not_panic() {
    let src = "@Backing(type=\"long\") enum Big { MAXV = 9223372036854775807, NEXT }";
    // No panic / abort is the assertion.
    let _ = generate_ok(src);
}

/// An empty `{}` initializer used to `unwrap()`-panic in five parser
/// positions where an aggregate initializer is not a valid value
/// (enumerator value, nested array element, annotation argument, named
/// annotation parameter, array dimension). Each must now surface as a
/// recoverable parse diagnostic. Reaching the assertions without aborting
/// is itself the regression guard: a revert reintroduces the panic and
/// fails the test. The legitimate empty-array initializer `int[] x = {}`
/// (the one position where `{}` is valid) must still parse + generate.
#[test]
fn empty_brace_initializer_is_rejected_not_panicked() {
    for src in [
        "enum E { A = {} }",                    // enumerator value
        "parcelable P { int[] x = {{}}; }",     // nested array element
        "@Foo({}) parcelable P { int x; }",     // annotation argument
        "@Foo(bar={}) parcelable P { int x; }", // named annotation parameter
        "parcelable P { int[{}] x; }",          // array dimension
    ] {
        assert!(
            !generate_ok(src),
            "empty `{{}}` initializer must error, not panic: {src:?}"
        );
    }
    assert!(
        generate_ok("parcelable P { int[] x = {}; }"),
        "a legitimate empty-array initializer must still generate"
    );
}

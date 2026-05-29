// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! AOSP `aidl_language.cpp:1211` rejects oneway methods
//! that return a value or carry `out`/`inout` parameters. Mirror that
//! at parse time so the diagnostic surfaces before codegen.

use rsbinder_aidl::{parse_document, AidlError, SourceContext};

fn expect_parse_error(input: &str) -> AidlError {
    let ctx = SourceContext::new("test.aidl", input);
    match parse_document(&ctx) {
        Err(e) => e,
        Ok(_) => panic!("expected parse error but parsing succeeded"),
    }
}

fn expect_parse_ok(input: &str) {
    let ctx = SourceContext::new("test.aidl", input);
    parse_document(&ctx).expect("parsing should succeed");
}

fn error_message(e: &AidlError) -> String {
    // Debug renders all inner diagnostics when the error is a
    // Multiple-variant aggregate; Display would show only the outer
    // "N error(s) occurred" wrapper.
    format!("{e:?}")
}

#[test]
fn oneway_method_with_non_void_return_is_rejected() {
    let err = expect_parse_error(
        r#"
interface IFoo {
    oneway int compute();
}
"#,
    );
    let msg = error_message(&err);
    assert!(
        msg.contains("oneway method 'compute' cannot return a value"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn oneway_interface_propagates_to_methods_with_non_void_return() {
    // AOSP: `oneway interface I { ... }` makes every method oneway.
    let err = expect_parse_error(
        r#"
oneway interface IFoo {
    int compute();
}
"#,
    );
    let msg = error_message(&err);
    assert!(
        msg.contains("oneway method 'compute' cannot return a value"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn oneway_method_with_out_param_is_rejected() {
    let err = expect_parse_error(
        r#"
interface IFoo {
    oneway void notify(out int[] result);
}
"#,
    );
    let msg = error_message(&err);
    assert!(
        msg.contains("oneway method 'notify' cannot have an 'out' parameter"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn oneway_method_with_inout_param_is_rejected() {
    let err = expect_parse_error(
        r#"
interface IFoo {
    oneway void notify(inout int[] buf);
}
"#,
    );
    let msg = error_message(&err);
    assert!(
        msg.contains("oneway method 'notify' cannot have an 'inout' parameter"),
        "unexpected error message: {msg}"
    );
}

#[test]
fn oneway_method_with_void_return_and_in_params_is_accepted() {
    expect_parse_ok(
        r#"
interface IFoo {
    oneway void notify(in int code, in String tag);
}
"#,
    );
}

#[test]
fn non_oneway_method_with_non_void_return_is_accepted() {
    expect_parse_ok(
        r#"
interface IFoo {
    int compute();
    void notify();
}
"#,
    );
}

#[test]
fn multiple_oneway_violations_are_all_reported() {
    // Mirrors AOSP error-collection style — one violation should not
    // mask the others in the same interface.
    let err = expect_parse_error(
        r#"
interface IFoo {
    oneway int badReturn();
    oneway void badOut(out int x);
}
"#,
    );
    let msg = error_message(&err);
    assert!(
        msg.contains("oneway method 'badReturn' cannot return a value"),
        "missing badReturn diagnostic in: {msg}"
    );
    assert!(
        msg.contains("oneway method 'badOut' cannot have an 'out' parameter"),
        "missing badOut diagnostic in: {msg}"
    );
}

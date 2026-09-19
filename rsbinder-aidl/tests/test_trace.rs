// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-9 AC-9.7: `trace` (AOSP `aidl --trace`) emits a method-name table
//! in `declare_binder_interface!`, and leaves the output untouched when off.
//!
//! The table follows AOSP `GetFunctionNames` / `GetMaxId`
//! (`system/tools/aidl/aidl_to_common.cpp`): indexed by method id, `""` for
//! an unused id, and cut off once more than 10 ids have been skipped. That
//! the generated table compiles and answers at runtime is
//! `tests/tests/transaction_names.rs` in the workspace test crate.

use rsbinder_aidl::render::{function_names, FnMembers};

fn generate(input: &str, trace: bool) -> String {
    generate_with(input, trace, false)
}

fn generate_with(input: &str, trace: bool, enabled_async: bool) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(enabled_async, false).with_trace(trace);
    gen.document(&document).expect("generate").1
}

const IMPLICIT: &str = r#"
package test.pkg;
interface IFoo {
    void ping();
    int add(int a, int b);
    oneway void notify(String s);
}
"#;

#[test]
fn trace_off_emits_no_table() {
    for enabled_async in [false, true] {
        let out = generate_with(IMPLICIT, false, enabled_async);
        assert!(!out.contains("function_names"), "{out}");
        assert!(!out.contains("__trace_client"), "{out}");
    }
}

/// Every proxy call, sync and async, opens its client span with the method's
/// own name and code. `getInterfaceVersion`/`getInterfaceHash` take the same
/// hook; `tests/tests/aidl_spans.rs` calls them at runtime.
#[test]
fn trace_on_opens_a_client_span_per_proxy_call() {
    for enabled_async in [false, true] {
        let out = generate_with(IMPLICIT, true, enabled_async);
        let copies = if enabled_async { 2 } else { 1 };
        for method in ["ping", "add", "notify"] {
            let hook = format!(
                "rsbinder::observe::__trace_client(\"test.pkg.IFoo\", \"{method}\", transactions::r#{method})"
            );
            assert_eq!(out.matches(&hook).count(), copies, "{method}:\n{out}");
        }
    }
}

#[test]
fn trace_on_emits_names_in_transaction_code_order() {
    let out = generate(IMPLICIT, true);
    let table = "\
            function_names: [
                \"ping\",
                \"add\",
                \"notify\",
            ],";
    assert!(out.contains(table), "{out}");
}

#[test]
fn trace_table_follows_the_vintf_stability_field() {
    let out = generate(
        r#"
package test.pkg;
@VintfStability
interface IFoo {
    void ping();
}
"#,
        true,
    );
    let stability = out.find("stability:").expect("stability field");
    let names = out.find("function_names:").expect("function_names field");
    assert!(
        stability < names,
        "the macro takes `function_names` after `stability`:\n{out}"
    );
}

#[test]
fn trace_on_interface_without_methods_emits_no_table() {
    let out = generate("package test.pkg;\ninterface IEmpty {}\n", true);
    assert!(!out.contains("function_names"), "{out}");
}

fn member(name: &str, code: Option<u32>) -> FnMembers {
    let mut m = FnMembers::new(name, code.unwrap_or(0));
    m.has_explicit_code = code.is_some();
    m
}

#[test]
fn explicit_ids_leave_empty_slots() {
    let names = function_names(&[member("a", Some(0)), member("b", Some(3))]);
    assert_eq!(names, ["a", "", "", "b"]);
}

/// 0 → 5 skips 4 ids and 5 → 30 skips 24 more: past AOSP `kMaxSkip` (10),
/// so the table stops at id 5 rather than growing to 31 entries.
#[test]
fn table_stops_once_more_than_ten_ids_are_skipped() {
    let names = function_names(&[
        member("a", Some(0)),
        member("b", Some(5)),
        member("c", Some(30)),
    ]);
    assert_eq!(names, ["a", "", "", "", "", "b"]);
}

#[test]
fn exactly_ten_skipped_ids_are_still_covered() {
    let names = function_names(&[member("a", Some(0)), member("b", Some(11))]);
    assert_eq!(names.len(), 12);
    assert_eq!(names[11], "b");
}

#[test]
fn no_methods_means_an_empty_table() {
    assert!(function_names(&[]).is_empty());
}

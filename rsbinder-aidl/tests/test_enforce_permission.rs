// SPDX-License-Identifier: Apache-2.0

//! `@EnforcePermission` codegen.
//!
//! Validates that the generated `on_transact` arm:
//!
//!   * Includes a `check_permission` call for each form
//!     (`@EnforcePermission("X")` / `(value = "X")` /
//!     `(allOf = {...})` / `(anyOf = {...})`).
//!   * Uses `&&` for `allOf` and `||` for `anyOf` — the short-circuit shape
//!     is rsbinder's own design: AOSP's C++ and Rust backends refuse
//!     permission annotations (`generate_cpp.cpp` `#error`), and its Java
//!     backend checks in a `<method>_enforcePermission()` helper.
//!   * Emits the deny branch (`Status::from(ExceptionCode::Security)` +
//!     `return Ok(())`) **before** any argument deserialization — the
//!     check appears earlier in the generated arm than the
//!     `let _arg_x: ... = _reader.read()` statements.
//!   * Leaves un-annotated methods byte-identical — the `IPlain` arm
//!     contains no `check_permission` reference.
//!
//! `@PermissionManuallyEnforced` and `@RequiresNoPermission` are documentation-only in AOSP's
//! AIDL: they let `aidl` enforce that every method declares its permission posture, but emit no
//! runtime check. rsbinder-aidl must recognize them (no `cargo:warning=`) and leave the generated
//! arm byte-identical to the un-annotated version.

fn generate(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).expect("generate").1
}

fn generate_async(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(true, false); // enabled_async = true
    gen.document(&document).expect("generate").1
}

fn arm_for(method_id: &str, generated: &str) -> String {
    // Arms sit one level inside `match _code { ... }`, so a brace counter finds the end.
    let needle = format!("transactions::r#{method_id} =>");
    let start = generated
        .find(&needle)
        .unwrap_or_else(|| panic!("arm `{method_id}` not found in:\n{generated}"));
    let after = &generated[start..];
    let mut depth = 0i32;
    let mut end = 0;
    for (i, ch) in after.char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    end = i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    after[..end].to_owned()
}

#[test]
fn enforce_permission_single_emits_one_check() {
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission("INTERNET")
    void doNet();
}
        "#,
    );
    let arm = arm_for("doNet", &out);
    let needle = "rsbinder::permission_controller::check_permission(_reader, \"INTERNET\")";
    assert!(
        arm.contains(needle),
        "missing single check `{needle}` in arm:\n{arm}"
    );
    assert!(
        arm.contains("rsbinder::ExceptionCode::Security"),
        "deny branch missing:\n{arm}"
    );
}

#[test]
fn enforce_permission_value_param_form_is_single() {
    // Named-parameter spelling of the single form: same emit as `@EnforcePermission("X")`.
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission(value = "INTERNET")
    void doNet();
}
        "#,
    );
    let arm = arm_for("doNet", &out);
    assert!(
        arm.contains("check_permission(_reader, \"INTERNET\")"),
        "{arm}"
    );
    // A single permission must not be mis-parsed into an allOf/anyOf join.
    assert!(
        !arm.contains(" && "),
        "single form must not emit AND:\n{arm}"
    );
    assert!(
        !arm.contains(" || "),
        "single form must not emit OR:\n{arm}"
    );
}

#[test]
fn enforce_permission_all_of_uses_and_short_circuit() {
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission(allOf = {"INTERNET", "ACCESS_NETWORK_STATE"})
    void doNetAndState();
}
        "#,
    );
    let arm = arm_for("doNetAndState", &out);
    assert!(
        arm.contains("check_permission(_reader, \"INTERNET\")"),
        "{arm}"
    );
    assert!(
        arm.contains("check_permission(_reader, \"ACCESS_NETWORK_STATE\")"),
        "{arm}"
    );
    // The `&&` short-circuit shape is rsbinder's own (see the module doc).
    assert!(arm.contains(" && "), "AllOf must join with `&&`:\n{arm}");
    assert!(!arm.contains(" || "), "AllOf must not emit `||`:\n{arm}");
}

#[test]
fn enforce_permission_any_of_uses_or_short_circuit() {
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission(anyOf = {"BLUETOOTH", "BLUETOOTH_ADMIN"})
    void doBluetooth();
}
        "#,
    );
    let arm = arm_for("doBluetooth", &out);
    assert!(
        arm.contains("check_permission(_reader, \"BLUETOOTH\")"),
        "{arm}"
    );
    assert!(
        arm.contains("check_permission(_reader, \"BLUETOOTH_ADMIN\")"),
        "{arm}"
    );
    assert!(arm.contains(" || "), "AnyOf must join with `||`:\n{arm}");
    assert!(!arm.contains(" && "), "AnyOf must not emit `&&`:\n{arm}");
}

#[test]
fn enforce_permission_check_runs_before_arg_deserialization() {
    // rsbinder design, not an AOSP port: a denied call must not deserialize (alloc, fd dup).
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission("INTERNET")
    void doNet(in String url, in int port);
}
        "#,
    );
    let arm = arm_for("doNet", &out);
    let check_pos = arm
        .find("check_permission(_reader, \"INTERNET\")")
        .expect("check_permission must be emitted");
    let read_pos = arm.find("_reader.read").unwrap_or_else(|| {
        panic!(
            "no argument deserialization found — the `_reader.read` marker \
             changed and this ordering guard is silently dead. arm:\n{arm}"
        )
    });
    assert!(
        check_pos < read_pos,
        "check_permission must precede argument deserialization. arm:\n{arm}"
    );
}

#[test]
fn methods_without_enforce_permission_get_no_check() {
    // Un-annotated methods get no permission scaffolding at all.
    let out = generate(
        r#"
package test;
interface IPlain {
    String echo(in String s);
}
        "#,
    );
    let arm = arm_for("echo", &out);
    assert!(
        !arm.contains("check_permission"),
        "un-annotated method leaked permission scaffolding:\n{arm}"
    );
    assert!(
        !arm.contains("ExceptionCode::Security"),
        "un-annotated method leaked deny branch:\n{arm}"
    );
}

/// Documentation-only annotation (see the module doc): output must match the plain interface.
#[test]
fn permission_manually_enforced_produces_byte_identical_codegen() {
    let plain = generate(
        r#"
package test;
interface IFoo {
    String echo(in String s);
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
interface IFoo {
    @PermissionManuallyEnforced
    String echo(in String s);
}
        "#,
    );
    // Anchor the content: "identical" must not pass as "identically empty".
    assert!(plain.contains("fn r#echo"), "{plain}");
    assert_eq!(
        plain, annotated,
        "@PermissionManuallyEnforced must not change codegen"
    );
}

#[test]
fn requires_no_permission_produces_byte_identical_codegen() {
    let plain = generate(
        r#"
package test;
interface IFoo {
    String echo(in String s);
}
        "#,
    );
    let annotated = generate(
        r#"
package test;
interface IFoo {
    @RequiresNoPermission
    String echo(in String s);
}
        "#,
    );
    // Anchor the content: "identical" must not pass as "identically empty".
    assert!(plain.contains("fn r#echo"), "{plain}");
    assert_eq!(
        plain, annotated,
        "@RequiresNoPermission must not change codegen"
    );
}

#[test]
fn permission_manually_enforced_is_recognized_no_warning() {
    let ctx = rsbinder_aidl::SourceContext::new(
        "test.aidl",
        r#"
package test;
interface IFoo {
    @PermissionManuallyEnforced
    @RequiresNoPermission
    String echo(in String s);
}
        "#,
    );
    let doc = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let manual_or_no_perm_warnings: Vec<_> = doc
        .warnings
        .iter()
        .filter(|w| {
            w.message.contains("@PermissionManuallyEnforced")
                || w.message.contains("@RequiresNoPermission")
        })
        .collect();
    assert!(
        manual_or_no_perm_warnings.is_empty(),
        "@PermissionManuallyEnforced / @RequiresNoPermission must be recognized \
         as known annotations: {manual_or_no_perm_warnings:?}"
    );
}

#[test]
fn mixed_methods_only_emit_check_for_annotated_arm() {
    // The check must not leak into other arms through shared template state.
    let out = generate(
        r#"
package test;
interface IMixed {
    @EnforcePermission("INTERNET")
    void doNet();

    String echo(in String s);
}
        "#,
    );
    let arm_net = arm_for("doNet", &out);
    let arm_echo = arm_for("echo", &out);
    assert!(
        arm_net.contains("check_permission(_reader, \"INTERNET\")"),
        "{arm_net}"
    );
    assert!(
        !arm_echo.contains("check_permission"),
        "echo arm should not contain check_permission:\n{arm_echo}"
    );
}

#[test]
fn enforce_permission_emitted_for_async_service() {
    // `Bn*Adapter` reuses the sync `on_transact`, so an async impl must hit the same check.
    let input = r#"
package test;
interface IFoo {
    @EnforcePermission("INTERNET")
    void doNet(in String url);
}
        "#;
    let out = generate_async(input);
    let arm = arm_for("doNet", &out);
    assert!(
        arm.contains("check_permission(_reader, \"INTERNET\")"),
        "async-enabled codegen must still emit the deny in on_transact:\n{arm}"
    );
    assert!(
        arm.contains("rsbinder::ExceptionCode::Security"),
        "async deny branch missing:\n{arm}"
    );
    // And it must precede argument deserialization, same as sync.
    let check_pos = arm
        .find("check_permission(_reader,")
        .expect("check present");
    let read_pos = arm.find("_reader.read").unwrap_or_else(|| {
        panic!("no `_reader.read` marker: the ordering guard would be dead. arm:\n{arm}")
    });
    assert!(
        check_pos < read_pos,
        "check must precede arg deserialization in async codegen:\n{arm}"
    );

    // Sanity: the async path is really generated, not just the sync output.
    assert!(
        out.contains("AsyncService"),
        "expected async service stub in async-enabled generation"
    );
}

/// AOSP Java `GeneratePermissionMethod`: an interface-level expression guards every method.
#[test]
fn interface_level_enforce_permission_guards_every_method() {
    let out = generate(
        r#"
package test;
@EnforcePermission(anyOf = {"A", "B"})
interface IFoo {
    void one();
    void two(in int x);
}
        "#,
    );
    for method in ["one", "two"] {
        let arm = arm_for(method, &out);
        assert!(
            arm.contains("check_permission(_reader, \"A\") || rsbinder::permission_controller"),
            "interface-level check missing from `{method}`:\n{arm}"
        );
    }
}

/// AOSP `AidlInterface::CheckValidPermissionAnnotations`.
#[test]
fn interface_and_method_permission_annotations_are_rejected() {
    for method_annotation in [
        "@EnforcePermission(\"B\")",
        "@RequiresNoPermission",
        "@PermissionManuallyEnforced",
    ] {
        let input = format!(
            "package test; @EnforcePermission(\"A\") interface IFoo {{ {method_annotation} void m(); }}"
        );
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", &input);
        let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
        let err = rsbinder_aidl::Generator::new(false, false)
            .document(&document)
            .expect_err("both annotated must be rejected");
        assert!(
            format!("{err:?}").contains("is also annotated"),
            "{method_annotation}: {err:?}"
        );
    }
}

/// AOSP `AidlAnnotation::EnforceExpression()`: `value`, else `anyOf`, else `allOf`.
#[test]
fn enforce_permission_parameters_pick_aosp_order_not_source_order() {
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission(anyOf = {"A", "B"}, value = "C")
    void single();
    @EnforcePermission(allOf = {"X", "Y"}, anyOf = {"A", "B"})
    void any();
}
        "#,
    );
    let arm = arm_for("single", &out);
    assert!(arm.contains("check_permission(_reader, \"C\")"), "{arm}");
    assert!(!arm.contains("check_permission(_reader, \"A\")"), "{arm}");
    let arm = arm_for("any", &out);
    assert!(arm.contains("check_permission(_reader, \"A\")"), "{arm}");
    assert!(!arm.contains("check_permission(_reader, \"X\")"), "{arm}");
}

/// AOSP `AidlAnnotation::CheckValid()` rejects unknown and ill-typed parameters.
#[test]
fn enforce_permission_unknown_or_ill_typed_parameter_is_rejected() {
    for args in [
        "value = 5, anyOf = {\"A\"}",
        "valeu = \"X\", anyOf = {\"A\"}",
        "anyOf = {\"A\"}, allOf = {1}",
    ] {
        let input =
            format!("package test; interface IFoo {{ @EnforcePermission({args}) void m(); }}");
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", &input);
        let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
        let err = rsbinder_aidl::Generator::new(false, false)
            .document(&document)
            .expect_err("malformed @EnforcePermission must be rejected");
        assert!(
            format!("{err:?}").contains("MalformedEnforcePermission"),
            "{args}: {err:?}"
        );
    }
}

/// AOSP `ParamValue` evaluates the value, so a string concatenation names one permission.
#[test]
fn enforce_permission_value_is_a_folded_constant_expression() {
    let out = generate(
        r#"
package test;
interface IFoo {
    @EnforcePermission("android.permission." + "X")
    void single();
    @EnforcePermission(anyOf = {"A" + "B", "C"})
    void any();
}
        "#,
    );
    let arm = arm_for("single", &out);
    assert!(
        arm.contains("check_permission(_reader, \"android.permission.X\")"),
        "{arm}"
    );
    let arm = arm_for("any", &out);
    assert!(arm.contains("check_permission(_reader, \"AB\")"), "{arm}");
}

/// AOSP refuses a redefined parameter (grammar) and a repeated annotation (`CheckValid`).
#[test]
fn enforce_permission_given_twice_is_rejected() {
    for (annotations, expected) in [
        (
            "@EnforcePermission(value = \"A\", value = \"B\")",
            "Trying to redefine parameter value.",
        ),
        (
            "@EnforcePermission(\"A\") @EnforcePermission(\"B\")",
            "'EnforcePermission' is repeated, but not allowed.",
        ),
    ] {
        let input = format!("package test; interface IFoo {{ {annotations} void m(); }}");
        let ctx = rsbinder_aidl::SourceContext::new("test.aidl", &input);
        let err = rsbinder_aidl::parse_document(&ctx).expect_err("must be rejected");
        assert!(
            format!("{err:?}").contains(expected),
            "{annotations}: {err:?}"
        );
    }
    // `@JavaPassthrough` is the one repeatable annotation in the AOSP schema.
    let input = "package test; interface IFoo { @JavaPassthrough(annotation = \"@A\") \
                 @JavaPassthrough(annotation = \"@B\") void m(); }";
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    rsbinder_aidl::parse_document(&ctx).expect("repeatable");
}

/// AOSP `method_decl`: `annotation_list ONEWAY type` annotates the return type with the list
/// before `oneway` and the one inside `type` alike, so either position guards the method.
#[test]
fn oneway_enforce_permission_is_checked_on_either_side_of_oneway() {
    for method in [
        "@EnforcePermission(\"INTERNET\") oneway void m();",
        "oneway @EnforcePermission(\"INTERNET\") void m();",
    ] {
        let out = generate(&format!("package test; interface IFoo {{ {method} }}"));
        let arm = arm_for("m", &out);
        assert!(
            arm.contains("check_permission(_reader, \"INTERNET\")"),
            "{method}: missing check in arm:\n{arm}"
        );
    }
}

#[test]
fn oneway_permission_annotation_after_oneway_meets_the_method_rules() {
    let input = "package test; @EnforcePermission(\"A\") interface IFoo { \
                 oneway @RequiresNoPermission void m(); }";
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let err = rsbinder_aidl::Generator::new(false, false)
        .document(&document)
        .expect_err("both annotated must be rejected");
    assert!(format!("{err:?}").contains("is also annotated"), "{err:?}");

    let input = "package test; interface IFoo { \
                 @EnforcePermission(\"A\") oneway @EnforcePermission(\"B\") void m(); }";
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let err = rsbinder_aidl::parse_document(&ctx).expect_err("must be rejected");
    assert!(
        format!("{err:?}").contains("'EnforcePermission' is repeated, but not allowed."),
        "{err:?}"
    );

    // The moved list still reaches the return-type checks: `void` takes no `@nullable`.
    let input = "package test; interface IFoo { oneway @nullable void m(); }";
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let err = rsbinder_aidl::Generator::new(false, false)
        .document(&document)
        .expect_err("nullable void must be rejected");
    assert!(
        format!("{err:?}").contains("cannot get nullable"),
        "{err:?}"
    );
}

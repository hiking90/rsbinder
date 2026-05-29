// SPDX-License-Identifier: Apache-2.0
//
// `@EnforcePermission` codegen.
//
// Validates that the generated `on_transact` arm:
//
//   * Includes a `check_permission` call for each form
//     (`@EnforcePermission("X")` / `(value = "X")` /
//     `(allOf = {...})` / `(anyOf = {...})`).
//   * Uses `&&` for `allOf` and `||` for `anyOf` — short-circuit shape
//     matches AOSP `generate_cpp.cpp::WriteEnforcePermissionCheck`.
//   * Emits the deny branch (`Status::from(ExceptionCode::Security)` +
//     `return Ok(())`) **before** any argument deserialization — the
//     check appears earlier in the generated arm than the
//     `let _arg_x: ... = _reader.read()` statements.
//   * Leaves un-annotated methods byte-identical (R2) — `IPlain` arm
//     contains no `check_permission` reference.

fn generate(input: &str) -> String {
    let ctx = rsbinder_aidl::SourceContext::new("test.aidl", input);
    let document = rsbinder_aidl::parse_document(&ctx).expect("parse");
    let gen = rsbinder_aidl::Generator::new(false, false);
    gen.document(&document).expect("generate").1
}

fn arm_for(method_id: &str, generated: &str) -> String {
    // Extract from `transactions::r#<method> => {` up to the matching
    // `}` brace of the arm. The arms are emitted single-level inside a
    // `match _code { ... }` so a simple brace counter is sufficient.
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
    let needle = "rsbinder::permission_controller::check_permission(\"INTERNET\")";
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
    // `@EnforcePermission(value = "X")` is the named-parameter spelling
    // of the single form — same emit as `@EnforcePermission("X")`.
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
    assert!(arm.contains("check_permission(\"INTERNET\")"), "{arm}");
    // Must NOT contain `&&` or `||` for the single form.
    assert!(
        !arm.contains(" && "),
        "single form must not emit AND:\n{arm}"
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
    assert!(arm.contains("check_permission(\"INTERNET\")"), "{arm}");
    assert!(
        arm.contains("check_permission(\"ACCESS_NETWORK_STATE\")"),
        "{arm}"
    );
    // The `&&` is the documented AOSP short-circuit shape.
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
    assert!(arm.contains("check_permission(\"BLUETOOTH\")"), "{arm}");
    assert!(
        arm.contains("check_permission(\"BLUETOOTH_ADMIN\")"),
        "{arm}"
    );
    assert!(arm.contains(" || "), "AnyOf must join with `||`:\n{arm}");
    assert!(!arm.contains(" && "), "AnyOf must not emit `&&`:\n{arm}");
}

#[test]
fn enforce_permission_check_runs_before_arg_deserialization() {
    // AOSP `WriteEnforcePermissionCheck` emits the deny early — before
    // any `_reader.read()` for arguments. We mirror that so a missing
    // permission cannot trigger argument deserialization side-effects
    // (allocations, fd dup, etc.). Regression check: the
    // `check_permission` call MUST appear before the first
    // `_reader.read*` for an annotated method.
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
        .find("check_permission(\"INTERNET\")")
        .expect("check_permission must be emitted");
    let read_pos = arm.find("_reader.read");
    if let Some(read_pos) = read_pos {
        assert!(
            check_pos < read_pos,
            "check_permission must precede argument deserialization. \
             arm:\n{arm}"
        );
    }
}

#[test]
fn methods_without_enforce_permission_get_no_check() {
    // R2 regression — un-annotated methods must produce byte-identical
    // generated code (no permission scaffolding). Confirms the
    // generator skips the check emit when annotation is absent.
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

/// `@PermissionManuallyEnforced` and `@RequiresNoPermission`
/// are documentation-only annotations in AOSP's AIDL — they exist so
/// `aidl` can enforce that *every* interface method declares its
/// permission posture, but neither emits runtime checks. rsbinder-aidl
/// must (a) recognize them (no `cargo:warning=` for typos) and (b) leave
/// the generated arm byte-identical to the un-annotated version (R2).
///
/// Locking-in test: compare generated output for the same interface
/// with and without the annotation — they MUST match exactly.
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
        "Phase B annotations must be recognized as known: {manual_or_no_perm_warnings:?}"
    );
}

#[test]
fn mixed_methods_only_emit_check_for_annotated_arm() {
    // An interface with both annotated and un-annotated methods must
    // emit the check ONLY in the annotated method's arm. Surfaces a
    // generator regression where the check would leak across arms via
    // shared template state.
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
        arm_net.contains("check_permission(\"INTERNET\")"),
        "{arm_net}"
    );
    assert!(
        !arm_echo.contains("check_permission"),
        "echo arm should not contain check_permission:\n{arm_echo}"
    );
}

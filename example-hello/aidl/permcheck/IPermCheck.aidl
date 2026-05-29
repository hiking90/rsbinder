// SPDX-License-Identifier: Apache-2.0
//
// Plan 4-2 Phase A end-to-end fixture: exercises all three
// `@EnforcePermission` forms in a single interface. Built alongside the
// main example-hello tree so the generated code is type-checked by
// `cargo build -p example-hello` — a quieter, more thorough check than
// the text-pattern asserts in
// `rsbinder-aidl/tests/test_enforce_permission.rs`.

package permcheck;

interface IPermCheck {
    @EnforcePermission("android.permission.INTERNET")
    boolean doSingle();

    @EnforcePermission(allOf = {"android.permission.INTERNET", "android.permission.ACCESS_NETWORK_STATE"})
    boolean doAllOf();

    @EnforcePermission(anyOf = {"android.permission.BLUETOOTH", "android.permission.BLUETOOTH_ADMIN"})
    boolean doAnyOf();

    /**
     * Phase D STAGE3 deny gate — guarded by a fabricated permission
     * name that `system_server` cannot have registered, so
     * `PermissionManagerService.checkPermission` returns `false` even
     * for root. The deny branch (`EX_SECURITY`) must fire BEFORE the
     * service implementation runs; if the client receives anything
     * other than `Status::Security`, the codegen leaked the check.
     */
    @EnforcePermission("rsbinder.test.permcheck.NONEXISTENT_PERMISSION")
    boolean doDenied();

    /** Un-annotated method: R2 byte-identical (no check emit). */
    String echo(in String message);
}

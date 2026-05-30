// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-16 Phase A hermetic fixture: an `@EnforcePermission` interface
// served over the RPC transport. Every guarded method must deny with
// `EX_SECURITY` regardless of process shape (the RPC root-bypass close),
// while the un-annotated method round-trips normally — proving the deny
// is scoped to the guarded arms, not the whole interface.

package rpcperm;

interface IRpcPermGuard {
    @EnforcePermission("android.permission.INTERNET")
    boolean doSingle();

    @EnforcePermission(allOf = {"android.permission.INTERNET", "android.permission.ACCESS_NETWORK_STATE"})
    boolean doAllOf();

    @EnforcePermission(anyOf = {"android.permission.BLUETOOTH", "android.permission.BLUETOOTH_ADMIN"})
    boolean doAnyOf();

    /** Un-annotated: must round-trip over RPC, no deny. */
    String echo(in String message);
}

// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-16 Phase B hermetic fixture: a service that reports the calling
// identity it observes *inside* its handler, so a test can assert that
// `get_calling_uid()` / `get_calling_pid()` / `is_handling_transaction()`
// work over the RPC transport (Unix RPC carries a kernel-vouched peer
// uid). `long` (i64) holds the unsigned uid without truncation.

package rpccaller;

interface IRpcCaller {
    /** Returns `get_calling_uid()` observed inside this handler. */
    long callingUid();

    /** Returns `get_calling_pid()` observed inside this handler. */
    long callingPid();

    /** Returns `is_handling_transaction()` observed inside this handler. */
    boolean handlingTransaction();

    /**
     * Classifies `calling_caller()` observed inside this handler
     * (Plan 2-16 Phase C): `"rpc-local:<uid>"` for a Unix RPC peer,
     * `"rpc-other"` for a uid-less RPC transport, `"kernel"` for kernel
     * binder, or `"none"` outside a transaction.
     */
    String callerKind();
}

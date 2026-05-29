// SPDX-License-Identifier: Apache-2.0
//
// STAGE3 smoke fixture for Plan 4-1: lets the server-side handler
// observe what the kernel binder driver delivers for calling identity
// (UID / PID / SELinux SID) and replay the AOSP clear/restore identity
// dance, then return the observations to the client as a single
// human-readable string.

package calling_identity;

interface ICallingIdentity {
    /**
     * Return a single line of the form:
     *
     *     uid=<u> pid=<p> sid=<s> explicit_pre=<bool> explicit_after_clear=<bool> explicit_after_restore=<bool>
     *
     * `sid` is the SELinux context (e.g. `u:r:shell:s0`) when the
     * server's binder was registered with
     * `BinderFeatures { set_requesting_sid: true, .. }` and the kernel
     * delivered BR_TRANSACTION_SEC_CTX; `<none>` otherwise.
     */
    String describeCaller();
}

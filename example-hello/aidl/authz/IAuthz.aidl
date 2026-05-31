// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-16 handler-side authorization example. The service `impl`
// authorizes each call by inspecting the transport-tagged caller
// (`rsbinder::calling_caller()`), and denies with `EX_SECURITY`. The same
// `impl` runs over kernel binder and RPC; see `bin/authz_service.rs`.

package authz;

interface IAuthz {
    /**
     * Allowed for any *identifiable local* caller (kernel binder, or a
     * Unix-domain RPC peer whose uid the kernel vouches for). A uid-less
     * RPC transport (vsock / TLS certificate / anonymous) is fail-closed.
     * Returns a human-readable description of the observed caller.
     */
    String whoami();

    /**
     * Requires the caller to be root (uid 0). A normal caller is denied
     * with `EX_SECURITY` — this demonstrates the deny path. (uid is the
     * kernel sender uid, or the Unix-RPC peer uid; never the fail-closed
     * sentinel, which is not 0.)
     */
    String adminOnly();
}

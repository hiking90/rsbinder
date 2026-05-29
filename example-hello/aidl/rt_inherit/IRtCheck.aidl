// SPDX-License-Identifier: Apache-2.0
//
// Plan 4-5 Phase D STAGE3 fixture. The server-side handler returns
// the scheduler policy it observed during the in-flight transaction
// — the client compares that against the policy it was running under
// to decide whether the kernel honored `FLAT_BINDER_FLAG_INHERIT_RT`.

package rt_inherit;

interface IRtCheck {
    /**
     * Returns `sched_getscheduler(0)` as observed inside the server's
     * `on_transact` body. Values mirror `<linux/sched.h>` constants:
     * `0 = SCHED_NORMAL`, `1 = SCHED_FIFO`, `2 = SCHED_RR`,
     * `3 = SCHED_BATCH`.
     */
    int reportSchedPolicy();
}

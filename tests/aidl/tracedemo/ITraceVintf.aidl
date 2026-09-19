// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-9 fixture: the `stability:` arm of `declare_binder_interface!`
// followed by `function_names:`, and the meta methods a versioned interface
// adds.
package tracedemo;

@VintfStability
interface ITraceVintf {
    void one() = 0;
    void two() = 2;
}

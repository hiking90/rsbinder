// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 2-19 P3 — the same interface is declared twice: here, and as a Rust
// trait carrying `#[rsbinder::interface]` in `tests/macro_cross.rs`. The two
// have to interoperate on the wire in both directions, which is a stronger
// claim than the textual golden tests inside `rsbinder-macros`: it also
// covers the descriptor, the transaction numbering, and the runtime.
package macrocross;

import macrocross.CrossCfg;
import macrocross.CrossMode;

interface IMacroCross {
    String echo(in String s);
    int add(in int a, in int b);
    CrossCfg apply(in CrossCfg cfg, in CrossMode mode);
    void fill(out int[] values);
    @nullable String maybe(in @nullable String s);
    oneway void ping();
}

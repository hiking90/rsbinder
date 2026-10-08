// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Cycles cut by one boxed field; the other fields stay inline (`tests/boxed_cycles.rs`).
package boxedcycles;

parcelable BoxedCycles {
    parcelable Outer {
        int value;
        @nullable(heap=true) Inner inner;
    }
    parcelable Inner {
        int value;
        Outer outer;
    }
    parcelable Tree {
        Node[3] nodes;
    }
    parcelable Node {
        int value;
        @nullable Tree owner;
    }
}

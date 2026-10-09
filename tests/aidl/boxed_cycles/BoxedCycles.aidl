// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Cycles cut by one boxed field; the other fields stay inline (`tests/boxed_cycles.rs`).
// `@nullable(heap=true)` boxes off a cycle too, as AOSP's Rust backend does.
package boxedcycles;

parcelable BoxedCycles {
    parcelable Leaf {
        int value;
    }
    parcelable HeapLeaf {
        @nullable(heap=true) Leaf leaf;
    }
    union HeapUnion {
        int number;
        @nullable(heap=true) Leaf leaf;
    }
    // The heap field cuts the cycle, so the bare `@nullable` back edge stays `Option<Front>`.
    parcelable Front {
        @nullable(heap=true) Back back;
    }
    parcelable Back {
        @nullable Front front;
    }
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

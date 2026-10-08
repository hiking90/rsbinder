// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Nested types named after std types the generated code spells (`tests/prelude_shadow.rs`).
// No nested `Box`: async-trait's expansion of the async service trait names `Box` bare.
package preludeshadow;

interface IShadowTypes {
    parcelable Option { int x; }
    parcelable Vec { int x; }
    int[] reverse(in int[] input, out int[] copy);
    @nullable int[] maybeInts(in @nullable int[] input);
    @nullable String[] names(in @nullable String[] input);
}

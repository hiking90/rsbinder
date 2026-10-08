// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Nested types named after std types the generated code spells (`tests/prelude_shadow.rs`).
package preludeshadow;

parcelable ShadowTypes {
    parcelable Option { int x; }
    parcelable Vec { int x; }
    parcelable Box { int x; }
    parcelable String { int x; }
    int[] values;
    @nullable(heap=true) ShadowTypes next;
    @utf8InCpp String text;
    @nullable @utf8InCpp String maybe;
    Option option;
}

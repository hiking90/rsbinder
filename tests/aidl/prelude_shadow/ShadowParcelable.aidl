// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Generated items named after std prelude items must not break the generated code
// (`tests/prelude_shadow.rs`).
package preludeshadow;

parcelable ShadowParcelable {
    const int Ok = 1;
    const int Err = 2;
    const int Some = 3;
    const int None = 4;
    parcelable Default {
        int x;
    }
    String text;
    Default nested;
    @nullable int[] values = {1, 2};
    @nullable String[] names = {"a"};
    ParcelableHolder holder;
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Generated items named after std prelude items must not break the generated code
// (`tests/prelude_shadow.rs`).
package preludeshadow;

union ShadowUnion {
    const int Ok = 1;
    const int Err = 2;
    const int Some = 3;
    const int None = 4;
    @nullable int[] values = {1, 2};
    IBinder binder;
    String text;
}

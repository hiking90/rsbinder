// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Generated items named after std prelude items must not break the generated code
// (`tests/prelude_shadow.rs`).
package preludeshadow;

interface IShadow {
    const int Ok = 0;
    const int Err = 1;
    const int Some = 2;
    const int None = 3;
    @EnforcePermission("android.permission.INTERNET") int guarded(int value);
    @nullable String maybe(@nullable String value);
}

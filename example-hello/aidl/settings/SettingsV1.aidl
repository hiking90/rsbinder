// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package settings;

/**
 * Version 1 of a stored record.
 *
 * Normally this file would simply *become* `SettingsV2.aidl` as the schema
 * grows. Both versions are kept here, under one build, so `serde_demo` can
 * play both sides of the version skew in a single process.
 */
@RustDerive(Clone=true, PartialEq=true)
parcelable SettingsV1 {
    String name = "unnamed";
    int volume = 50;
    Mode mode = Mode.AUTO;
    Endpoint endpoint;
    String[] tags;
    @nullable String note;
}

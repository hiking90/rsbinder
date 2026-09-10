// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package settings;

/**
 * Version 2: {@link SettingsV1} with two fields appended, and nothing else
 * touched.
 *
 * Appending is the only edit a positional format absorbs. Reordering or
 * removing a field changes what the bytes at each offset mean, and no reader
 * can detect that — it just decodes the wrong values.
 */
@RustDerive(Clone=true, PartialEq=true)
parcelable SettingsV2 {
    String name = "unnamed";
    int volume = 50;
    Mode mode = Mode.AUTO;
    Endpoint endpoint;
    String[] tags;
    @nullable String note;

    // Appended in v2.
    int retries = 3;
    long updatedAtMillis;
}

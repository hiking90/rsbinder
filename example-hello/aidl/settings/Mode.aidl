// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package settings;

/** Backed by `int`, so it is four bytes wherever it is stored. */
@Backing(type="int")
enum Mode {
    OFF = 0,
    ON = 1,
    AUTO = 2,
}

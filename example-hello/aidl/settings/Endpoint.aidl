// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package settings;

/** A nested parcelable, to show that nesting survives storage too. */
@RustDerive(Clone=true, PartialEq=true)
parcelable Endpoint {
    String host = "localhost";
    int port = 8080;
}

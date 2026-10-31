// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 17 K-22 fixture (`tests/sync_only_codegen.rs`): generated with
// `Builder::set_async_support(false)` and built against rsbinder's default
// features, where `async` is on.
package synconly;

interface ISyncOnly {
    int add(int a, int b);

    String echo(in String s);
}

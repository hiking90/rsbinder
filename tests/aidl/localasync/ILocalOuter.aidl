// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 17 K-4 fixture (`tests/local_async_kernel.rs`): a kernel service whose handler
// calls a sync-only local `ILocalSvc` through its async view, inline inside the
// transaction, and reports what the inner handler saw.
package localasync;

interface ILocalOuter {
    String probeInner();
}

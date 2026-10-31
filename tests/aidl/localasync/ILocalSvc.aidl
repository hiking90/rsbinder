// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 17 K-4 fixture (`tests/local_async_kernel.rs`, `tests/local_async_no_kernel.rs`):
// a service published sync-only and called through its async view, which drives the
// local binder through the in-process transport. Each method reports what the
// service's handler saw.
package localasync;

interface ILocalSvc {
    // `calling_caller()` in the handler: "kernel:<pid>", "inprocess:<uid>:<pid>",
    // "rpc", or "none".
    String callerKind();

    // `is_handling_transaction()` in the handler.
    boolean handlingTransaction();

    // `std::thread::current().id()` of the handler, as `Debug` prints it.
    String threadId();

    // The handler thread's strict-mode policy and work source uid: "<policy hex>:<uid>".
    String threadState();

    // Returns `b` unchanged.
    IBinder echoBinder(IBinder b);

    // Everything `fd` holds, read to its end.
    String readFd(in ParcelFileDescriptor fd);

    oneway void bump();

    // How many `bump` calls the service has run.
    int bumps();

    @EnforcePermission("android.permission.INTERNET")
    boolean guarded();
}

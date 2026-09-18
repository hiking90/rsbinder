// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-5 fixture: a service that hands out a cancellation transport
// and reports what its own work loop saw.
//
// `startJob` returns the transport as a bare `IBinder` rather than
// importing `android.os.ICancellationSignal`: every crate that compiles
// that AIDL gets its own Rust type, so importing it here would make a
// second one incompatible with `rsbinder::cancel::ICancellationSignal`
// (the wire is identical either way). The caller cancels it with
// `rsbinder::cancel::cancel_remote`.
package canceldemo;

interface ICancelDemo {
    IBinder startJob(String job);
    boolean wasCanceled(String job);
}

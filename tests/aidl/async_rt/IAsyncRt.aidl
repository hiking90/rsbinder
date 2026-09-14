// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Minimal AIDL fixture for plan 11 Phase A (`tests/async_runtime.rs`):
// a local async-backed binder, called from every runtime position the
// `BinderAsyncRuntime` adapter can be reached from. Needs no kernel
// device and no RPC transport — the handle never leaves the process.
package asyncrt;

interface IAsyncRt {
    // Returns "echo:<msg>". The async service impl awaits before it
    // answers, so a stalled timer driver shows up as a call that never
    // returns rather than a wrong value.
    String echo(String msg);
}

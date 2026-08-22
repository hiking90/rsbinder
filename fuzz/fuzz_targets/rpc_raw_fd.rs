// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → the **bare** RPC fd decode
//! (`read_raw_fd`, the handwritten `android.utils.IMemoryHeap` wire) in
//! both the R34/v0 bare-index and the v1+ `[TYPE|idx]` + hostile
//! object-position-table profiles, with no received fds. Property: no
//! panic / UB / fd leak; every forged input is a clean `Err`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::file_descriptor::__fuzz_rpc_raw_fd(data);
});

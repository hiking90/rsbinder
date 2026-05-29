// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → RPC `Unix`-mode FD-table decode with
//! no received fds. Property: no panic /
//! UB / fd leak; an OOB or dangling fd-table index is a clean `Err`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::file_descriptor::__fuzz_rpc_fd_index(data);
});

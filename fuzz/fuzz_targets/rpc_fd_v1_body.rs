// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes + a hostile object-position table
//! through the **v1+ AOSP-shape** RPC FD decode
//! (`[not-null|hasComm|TYPE|idx]` + strict `binary_search`), no
//! received fds (subplan 2-11 Phase B / V4). Property: no panic / UB /
//! fd leak — a forged/unsorted/absent position, wrong `TYPE`, non-zero
//! `hasComm`, or dangling index is a clean `Err`.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::file_descriptor::__fuzz_rpc_fd_index_v1(data);
});

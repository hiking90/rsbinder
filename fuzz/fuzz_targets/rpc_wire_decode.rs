// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → R34 wire message decoder
//! (`plan/2-2` S-b / 2-2.b4 / V4). Property: no panic, no OOM, no UB;
//! every length/offset bounds-checked; no pre-alloc past MAX_FRAME_LEN.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::rpc::wire::__fuzz_decode_wire(data);
});

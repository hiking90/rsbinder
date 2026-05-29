// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → RpcWireAddress parse path
//! Property: no panic/OOM/UB, the 32-byte
//! address parse is fully bounds-checked even for short input.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::rpc::wire::__fuzz_decode_address(data);
});

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: feed arbitrary bytes through the RPC frame deframer
//! (the exact path `RpcTransport::recv_frame` uses for stream
//! backends). Property under test: **no panic, no OOM, no infinite
//! loop, no UB** on any input — a declared length above
//! `MAX_FRAME_LEN` must be rejected before allocation.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // Decode-only entrypoint shared with the deterministic regression
    // test. Returning Err is fine; panicking / OOM / hang is not.
    let _ = rsbinder::rpc::transport::__fuzz_decode_frame(data);
});

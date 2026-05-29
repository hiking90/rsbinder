// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → session preamble + message decode
//! Property: no panic/OOM/hang; malformed
//! negotiation/handshake bytes are rejected, never trusted.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::rpc::wire::__fuzz_session_handshake(data);
});

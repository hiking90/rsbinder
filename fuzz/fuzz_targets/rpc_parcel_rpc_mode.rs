// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → **RPC-mode** `Parcel` deserialization
//! (`plan/2-2` §6.3 / V4 — the third 2-2 fuzz target). RPC newly drives
//! the AIDL parcel deserializers with untrusted *socket* bytes (the
//! kernel path never did — the driver validated senders). Property: no
//! panic / OOM / UB / unbounded pre-allocation; every length-driven
//! allocation is bounded by the bytes actually present (T1-3).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::rpc::__fuzz_decode_rpc_parcel(data);
});

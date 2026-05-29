// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → **RPC-mode** `Parcel` deserialization
//! RPC newly drives
//! the AIDL parcel deserializers with untrusted *socket* bytes (the
//! kernel path never did — the driver validated senders). Property: no
//! panic / OOM / UB / unbounded pre-allocation; every length-driven
//! allocation is bounded by the bytes actually present.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::rpc::__fuzz_decode_rpc_parcel(data);
});

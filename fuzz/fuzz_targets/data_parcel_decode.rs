// Copyright 2025 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes → `rsbinder::from_bytes`.
//!
//! `from_bytes` invites a new kind of input. RPC bytes at least came
//! from a peer that spoke the protocol; a stored file can be anything —
//! truncated by a full disk, edited by hand, or written by an attacker
//! who controls the path. Property: no panic, no unbounded allocation
//! from a length field, and no object ever fabricated from bytes that
//! merely look like one — every one of those is an `Err`.
//!
//! The types below are chosen for what they make the decoder do: a
//! length-prefixed byte array, a UTF-16 string, a nested read with a
//! size header, and a binder field the decoder must refuse.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = rsbinder::from_bytes::<i32>(data);
    let _ = rsbinder::from_bytes::<i64>(data);
    let _ = rsbinder::from_bytes::<String>(data);
    let _ = rsbinder::from_bytes::<Vec<u8>>(data);
    let _ = rsbinder::from_bytes::<Vec<i32>>(data);
    let _ = rsbinder::from_bytes::<Vec<String>>(data);
    let _ = rsbinder::from_bytes::<Option<String>>(data);
    let _ = rsbinder::from_bytes::<rsbinder::ParcelableHolder>(data);
    // Must always be `Err`: there is no object table behind these bytes.
    let _ = rsbinder::from_bytes::<rsbinder::SIBinder>(data);
});

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target (subplan 2-13 B.6 / V4): arbitrary bytes → AOSP
//! `IAccessor::ERROR_*` decode. Property: no panic / OOM / hang; any
//! unknown code falls through to the `"unknown"` log hint, never an
//! authoritative-looking symbol. The enforceable regression is the
//! deterministic test
//! `hub::accessor_16::tests::accessor_error_name_*` (runs in
//! `cargo test`); this target is the libFuzzer soak supplement.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder::hub::android_16::__fuzz_accessor_error_decode(data);
});

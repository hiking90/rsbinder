// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Fuzz target: arbitrary bytes as topic memory → a subscriber's receive calls.
//!
//! A subscriber of `rsbinder-topic` reads memory the publisher wrote and can
//! still write, so it reads it as untrusted input: the counters, and the
//! length in every record's header. The input here is such memory as an
//! image — 64 bytes of header, then the ring — held in a heap allocation of
//! exactly that size. Property: no receive call panics, none copies from
//! outside the ring (the sanitizer's to report), and each ends within the
//! bound on its work, which the entry point asserts.
//!
//! The image does not change while it is read, so this covers a publisher
//! that wrote anything at all and stopped. A publisher that keeps writing is
//! the unit tests' and the loom models' subject.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    rsbinder_topic::fuzzing::recv_image(data);
});

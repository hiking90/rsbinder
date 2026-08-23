// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! What the macros refuse, as the user sees it.
//!
//! The unit tests next to each macro assert the message; these assert that it
//! arrives as a compile error pointing at the offending token, which is the
//! part a user actually experiences.

#[test]
fn rejections_are_compile_errors() {
    trybuild::TestCases::new().compile_fail("tests/ui/*.rs");
}

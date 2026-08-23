// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Library half of `rsbinder-tools`.
//!
//! The CLI binaries (`rsb_hub`, `rsb_device`) stay thin; anything worth
//! unit-testing on its own lives here.

pub mod nss;

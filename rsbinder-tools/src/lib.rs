// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Library half of `rsbinder-tools`.
//!
//! The CLI binaries (`rsb_hub`, `rsb_device`) stay thin; anything worth
//! unit-testing on its own lives here. Currently that is the service
//! manager's configuration — access-control policy today, service
//! declarations next — see [`config`] and `plans/6-1-hub-access-control.md`.

pub mod config;
pub mod nss;

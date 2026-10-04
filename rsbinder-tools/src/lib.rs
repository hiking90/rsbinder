// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Library half of `rsbinder-tools`.
//!
//! The CLI binaries (`rsb_hub`, `rsb_service`, `rsb_device`) stay thin;
//! anything worth unit-testing on its own lives here. Today that is the
//! service manager's configuration ([`config`] — access control, service
//! declarations, on-demand activation) and its readiness notification
//! ([`notify`]). See `plans/6-rsb-hub-linux.md`.

// Library code returns errors; tests may unwrap (plan 13).
#![cfg_attr(not(test), deny(clippy::unwrap_used))]
// Every allow outside tests says why (plan 13-1).
#![cfg_attr(not(test), deny(clippy::allow_attributes_without_reason))]

pub mod config;
pub mod notify;
pub mod nss;

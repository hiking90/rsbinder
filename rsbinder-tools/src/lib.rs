// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Library half of `rsbinder-tools`.
//!
//! The CLI binaries (`rsb_hub`, `rsb_service`, `rsb_device`) stay thin;
//! anything worth unit-testing on its own lives here. Today that is the
//! service manager's configuration ([`config`] — access control, service
//! declarations, on-demand activation) and its readiness notification
//! ([`notify`]). See `plans/6-rsb-hub-linux.md`.

pub mod config;
pub mod notify;
pub mod nss;

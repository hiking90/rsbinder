// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The topic fleet (plan 15 fleet test): many `topic_node` processes publishing and subscribing
//! over kernel binder while `tests/topic_fleet.rs` starts, stops and kills them.
//!
//! What both sides share lives here: the line a node prints for each event ([`event`]), the
//! record a publisher writes ([`record`]), the checks of what a subscription receives
//! ([`judge`]), and the one place a node finds and joins a topic ([`transport`]). A node prints an event only when something changes (a connection, an end, a
//! violation, a count at most once a second), never one per record.

pub mod event;
pub mod judge;
pub mod payload;
pub mod record;
pub mod spec;
pub mod transport;

/// `CLOCK_MONOTONIC` in nanoseconds: one clock for every process of the machine, so the
/// controller can measure from its own action to an event a node stamped.
pub fn now_ns() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    now.tv_sec as u64 * 1_000_000_000 + now.tv_nsec as u64
}

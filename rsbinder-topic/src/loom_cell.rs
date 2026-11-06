// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A loom model's plain memory: a read and a write of a `u64` in loom's `UnsafeCell`. Loom checks
//! each access against every other access of the cell before it runs, and fails the model on one
//! that does not happen before or after it, so neither access can race and both are safe. Both
//! are `#[track_caller]`, as loom's `with` and `with_mut` are: a failure names the model's line.

use loom::cell::UnsafeCell;

/// The value in `cell`.
#[track_caller]
pub(crate) fn read(cell: &UnsafeCell<u64>) -> u64 {
    // SAFETY: loom has checked that no write of `cell` races with this read.
    cell.with(|value| unsafe { *value })
}

/// `new` in `cell`.
#[track_caller]
pub(crate) fn write(cell: &UnsafeCell<u64>, new: u64) {
    // SAFETY: loom has checked that no other access of `cell` races with this write.
    cell.with_mut(|value| unsafe { *value = new });
}

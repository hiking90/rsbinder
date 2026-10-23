// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The order a wait for room rests on, under loom (plan 15 AC-15.3.12): a subscriber copies a
//! record and then stores its `read` with `Release`; the publisher loads that `read` with
//! `Acquire` and only then writes over the record. The ring's bytes are atomic words in the
//! crate, so a `read` stored `Relaxed` there costs a record seen as overwritten, not undefined
//! behavior; the model holds the record in loom's `UnsafeCell`, which fails a write that does not
//! happen after every read of it, so the order is checked as plain memory would need it. The
//! flawed model stores `read` `Relaxed` and fails. Run: see `Cargo.toml` (`--cfg loom`).

// The std `Arc`: one of loom's dropped while a failed model unwinds calls into loom and aborts.
use std::sync::Arc;

use loom::cell::UnsafeCell;
use loom::sync::atomic::{AtomicU64, Ordering};
use loom::thread;

/// A ring of one record: the record, the records committed, and the records read.
struct Model {
    record: UnsafeCell<u64>,
    write: AtomicU64,
    read: AtomicU64,
    read_order: Ordering,
}

// SAFETY: the record is what loom checks: every access is through `with` or `with_mut`.
unsafe impl Sync for Model {}

/// Record 2 written over record 1 once it is read; the subscriber stores `read` with `read_order`.
fn a_record_is_written_over_only_after_it_is_read(read_order: Ordering) {
    loom::model(move || {
        let model = Arc::new(Model {
            record: UnsafeCell::new(0),
            write: AtomicU64::new(0),
            read: AtomicU64::new(0),
            read_order,
        });
        let subscriber = {
            let model = model.clone();
            thread::spawn(move || {
                while model.write.load(Ordering::Acquire) < 1 {
                    thread::yield_now();
                }
                // SAFETY: a committed record; loom checks that nothing writes it meanwhile.
                let copied = model.record.with(|record| unsafe { *record });
                assert_eq!(copied, 1, "a record written over before it was read");
                model.read.store(1, model.read_order);
            })
        };
        // SAFETY: before any commit, so no subscriber reads it; loom checks that it is so.
        model.record.with_mut(|record| unsafe { *record = 1 });
        model.write.store(1, Ordering::Release);
        while model.read.load(Ordering::Acquire) < 1 {
            thread::yield_now();
        }
        // SAFETY: the subscriber has read record 1; loom checks that its read came first.
        model.record.with_mut(|record| unsafe { *record = 2 });
        model.write.store(2, Ordering::Release);
        subscriber.join().unwrap();
    });
}

#[test]
fn loom_backpressure_a_record_is_written_over_only_after_it_is_read() {
    a_record_is_written_over_only_after_it_is_read(Ordering::Release);
}

#[test]
#[should_panic(expected = "Causality violation")]
fn loom_backpressure_a_read_stored_relaxed_lets_the_overwrite_race_the_copy() {
    a_record_is_written_over_only_after_it_is_read(Ordering::Relaxed);
}

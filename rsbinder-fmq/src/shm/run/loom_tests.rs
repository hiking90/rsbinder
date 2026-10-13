// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The scheme `Run::store_release` and `Run::load_acquire` follow, as a loom model on one data
//! word: a `Release` store, a `Relaxed` load, an `Acquire` fence. It checks the scheme and does
//! not run `Run`'s code; the test that does is in `run.rs` and decides under Miri
//! (`a_byte_read_orders_the_reader_after_the_writers_announcement`). Run: see `Cargo.toml`
//! (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Writer: `end` then the word (`store`). Reader: the word, a fence if `acquire`, then `end`.
fn a_new_word_means_a_new_announcement(store: Ordering, acquire: bool) {
    loom::model(move || {
        let end = Arc::new(AtomicUsize::new(0));
        let data = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (end, data) = (end.clone(), data.clone());
            thread::spawn(move || {
                end.store(1, Ordering::Relaxed);
                data.store(1, store);
            })
        };
        let word = data.load(Ordering::Relaxed);
        if acquire {
            fence(Ordering::Acquire);
        }
        let announced = end.load(Ordering::Relaxed);
        assert!(announced >= word, "a new word with the old announcement");
        writer.join().unwrap();
    });
}

#[test]
fn loom_release_stores_and_the_acquire_fence_keep_the_order() {
    a_new_word_means_a_new_announcement(Ordering::Release, true);
}

#[test]
#[should_panic(expected = "a new word with the old announcement")]
fn loom_the_order_is_lost_with_relaxed_stores() {
    a_new_word_means_a_new_announcement(Ordering::Relaxed, true);
}

#[test]
#[should_panic(expected = "a new word with the old announcement")]
fn loom_the_order_is_lost_without_the_readers_fence() {
    a_new_word_means_a_new_announcement(Ordering::Release, false);
}

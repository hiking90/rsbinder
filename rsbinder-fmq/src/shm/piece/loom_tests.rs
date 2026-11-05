// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The scheme `Bytes::store` and `Bytes::load_acquire` follow, as a loom model on two data words:
//! a release fence, `Relaxed` stores, `Relaxed` loads, an `Acquire` fence. It checks the scheme
//! and does not run `Bytes`' code; the test that does is in `piece/tests.rs` and decides under
//! Miri (`a_byte_read_orders_the_reader_after_the_writers_announcement`). Run: see `Cargo.toml`
//! (`--cfg loom`).

use loom::sync::atomic::{fence, AtomicUsize, Ordering};
use loom::sync::Arc;
use loom::thread;

/// Writer: `end`, a fence if `release`, then the word. Reader: the word, a fence if `acquire`,
/// then `end`.
fn a_new_word_means_a_new_announcement(release: bool, acquire: bool) {
    loom::model(move || {
        let end = Arc::new(AtomicUsize::new(0));
        let data = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (end, data) = (end.clone(), data.clone());
            thread::spawn(move || {
                end.store(1, Ordering::Relaxed);
                if release {
                    fence(Ordering::Release);
                }
                data.store(1, Ordering::Relaxed);
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
fn loom_the_two_fences_keep_the_order() {
    a_new_word_means_a_new_announcement(true, true);
}

#[test]
#[should_panic(expected = "a new word with the old announcement")]
fn loom_the_order_is_lost_without_the_writers_fence() {
    a_new_word_means_a_new_announcement(false, true);
}

#[test]
#[should_panic(expected = "a new word with the old announcement")]
fn loom_the_order_is_lost_without_the_readers_fence() {
    a_new_word_means_a_new_announcement(true, false);
}

// One fence orders what came before it, not the stores after it among themselves: loom does run
// the state the ring's readers must not rest on.
#[test]
#[should_panic(expected = "the second word without the first")]
fn loom_the_words_after_one_fence_come_in_no_order() {
    loom::model(|| {
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let writer = {
            let (first, second) = (first.clone(), second.clone());
            thread::spawn(move || {
                fence(Ordering::Release);
                first.store(1, Ordering::Relaxed);
                second.store(1, Ordering::Relaxed);
            })
        };
        let seen_second = second.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        let seen_first = first.load(Ordering::Relaxed);
        assert!(
            seen_first >= seen_second,
            "the second word without the first"
        );
        writer.join().unwrap();
    });
}

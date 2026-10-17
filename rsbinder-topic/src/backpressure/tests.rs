// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;

use super::*;

/// One subscription's `read`, as another thread moves it.
struct Read(AtomicU64);

impl Readers for Read {
    fn lowest_read(&self) -> u64 {
        self.0.load(Ordering::Acquire)
    }
}

fn after(timeout: Duration) -> Option<Instant> {
    Instant::now().checked_add(timeout)
}

#[test]
fn room_already_made_or_with_no_subscriber_is_no_wait() {
    let read = Read(AtomicU64::new(64));
    assert_eq!(wait_for(&read, 64, after(Duration::ZERO)), Room::Made);
    let none = Read(AtomicU64::new(u64::MAX));
    let began = Instant::now();
    assert_eq!(wait_for(&none, u64::MAX - 1, None), Room::Made);
    assert!(began.elapsed() < Duration::from_millis(100));
}

#[test]
fn a_wait_ends_when_the_subscriber_reads_up_to_what_it_needs() {
    let read = Read(AtomicU64::new(0));
    thread::scope(|scope| {
        scope.spawn(|| {
            for at in [8, 16, 24] {
                thread::sleep(Duration::from_millis(5));
                read.0.store(at, Ordering::Release);
            }
        });
        let began = Instant::now();
        let room = wait_for(&read, 24, after(Duration::from_secs(10)));
        let took = began.elapsed();
        assert_eq!(room, Room::Made);
        assert!(took < Duration::from_secs(5), "{took:?}");
    });
}

#[test]
fn a_wait_with_no_progress_gives_up_at_its_deadline_and_not_before() {
    let read = Read(AtomicU64::new(0));
    let timeout = Duration::from_millis(30);
    let began = Instant::now();
    assert_eq!(wait_for(&read, 8, after(timeout)), Room::TimedOut);
    let waited = began.elapsed();
    assert!(waited >= timeout, "{waited:?}");
    assert!(waited < timeout + Duration::from_secs(1), "{waited:?}");
}

#[test]
fn a_zero_timeout_gives_up_without_sleeping() {
    let read = Read(AtomicU64::new(0));
    let began = Instant::now();
    assert_eq!(wait_for(&read, 8, after(Duration::ZERO)), Room::TimedOut);
    assert!(began.elapsed() < Duration::from_millis(100));
}

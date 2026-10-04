// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Heap allocations a kernel-mode `Parcel` makes, counted by a global allocator.
//!
//! A parcel that carries a proxy handle keeps that proxy strong until it drops
//! (`Parcel::kernel_pinned`, AOSP `acquire_object`). The pin list is created empty,
//! so a parcel that never writes a handle — most transactions, every received
//! parcel — pays nothing for it. These tests fail if the list is ever allocated
//! eagerly, which would add one `malloc`/`free` pair to every parcel.
//!
//! The counter is thread-local: the harness's other threads (and the binder
//! thread pool, in the ignored case) do not perturb the tally. Each case writes
//! once before counting so that one-time lazy initialisation on the path (SDK
//! level, `ProcessState` caches) is not attributed to the parcel.
//!
//! `a_parcel_carrying_a_handle_allocates_the_pin_list_once` needs `/dev/binder`,
//! a running `rsb_hub` and `test_service` (see `tests/README.md`), so it is
//! `#[ignore]`d: `cargo test -p rsbinder --test parcel_alloc -- --ignored`.

#![cfg_attr(
    target_os = "android",
    allow(
        clippy::missing_const_for_thread_local,
        reason = "android-only false positive, even on thread_locals already using `const { .. }`"
    )
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use rsbinder::{Parcel, ParcelFileDescriptor};

struct Counting;

thread_local! {
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

fn bump() {
    let _ = ALLOCS.try_with(|c| c.set(c.get() + 1));
}

// SAFETY: every method forwards to `System` unchanged; only a thread-local counter is added.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract, which `System` shares.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        bump();
        // SAFETY: as for `alloc`.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        bump();
        // SAFETY: `ptr` came from this allocator, i.e. from `System`, with `layout`.
        unsafe { System.realloc(ptr, layout, new_size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: `ptr` came from this allocator, i.e. from `System`, with `layout`.
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

/// Allocations `f` makes on this thread, dropping its result inside the window.
fn count<R>(f: impl FnOnce() -> R) -> usize {
    let before = ALLOCS.with(Cell::get);
    drop(f());
    ALLOCS.with(Cell::get) - before
}

#[test]
fn a_scalar_only_parcel_allocates_its_data_buffer_only() {
    let write = || {
        let mut p = Parcel::with_capacity(256);
        p.write(&1i32).unwrap();
        p.write(&2u64).unwrap();
        p
    };
    drop(write());
    // Data buffer only: no objects table, no pin list.
    assert_eq!(count(write), 1);
}

#[test]
fn a_parcel_carrying_an_fd_allocates_no_pin_list() {
    let pfd = ParcelFileDescriptor::new(std::fs::File::open("/dev/null").unwrap());
    let write = || {
        let mut p = Parcel::with_capacity(256);
        p.write(&1i32).unwrap();
        p.write(&pfd).unwrap();
        p
    };
    drop(write());
    // Data + objects table + `kernel_fds` (one fd); an eager pin list would make it 4.
    assert_eq!(count(write), 3);
}

#[test]
#[ignore = "needs /dev/binder, a running rsb_hub and test_service"]
fn a_parcel_carrying_a_handle_allocates_the_pin_list_once() {
    rsbinder::ProcessState::init_default().expect("init_default");
    let proxy = rsbinder::hub::check_service("android.aidl.tests.ITestService")
        .expect("test_service is not registered");
    assert!(proxy.as_proxy().is_some(), "expected a remote binder");

    let write = || {
        let mut p = Parcel::with_capacity(256);
        p.write(&1i32).unwrap();
        p.write(&proxy).unwrap();
        p
    };
    drop(write());
    // Data buffer + objects table + pin list, each allocated once.
    assert_eq!(count(write), 3);
}

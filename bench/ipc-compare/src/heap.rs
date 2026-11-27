//! The process's global allocator: the system's, counting live allocations and bytes. Each
//! thread adds to a slot of its own cache line, so threads do not contend on every allocation.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};

const SLOTS: usize = 64;

#[repr(align(128))]
struct Slot {
    allocs: AtomicI64,
    bytes: AtomicI64,
}

static TABLE: [Slot; SLOTS] = [const { Slot { allocs: AtomicI64::new(0), bytes: AtomicI64::new(0) } }; SLOTS];
static NEXT: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    // Const, with no destructor: usable inside the allocator, also while a thread exits.
    static MINE: Cell<usize> = const { Cell::new(usize::MAX) };
}

fn count(allocs: i64, bytes: i64) {
    let index = MINE
        .try_with(|mine| {
            if mine.get() == usize::MAX {
                mine.set(NEXT.fetch_add(1, Ordering::Relaxed) % SLOTS);
            }
            mine.get()
        })
        .unwrap_or(0);
    let slot = &TABLE[index];
    slot.allocs.fetch_add(allocs, Ordering::Relaxed);
    slot.bytes.fetch_add(bytes, Ordering::Relaxed);
}

pub struct Counting;

// SAFETY: every call is passed to `System` unchanged; the counting touches no allocation.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc`, passed on.
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            count(1, layout.size() as i64);
        }
        ptr
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's contract for `alloc_zeroed`, passed on.
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            count(1, layout.size() as i64);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: the caller's contract for `dealloc`, passed on.
        unsafe { System.dealloc(ptr, layout) };
        count(-1, -(layout.size() as i64));
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: the caller's contract for `realloc`, passed on.
        let new = unsafe { System.realloc(ptr, layout, new_size) };
        if !new.is_null() {
            count(0, new_size as i64 - layout.size() as i64);
        }
        new
    }
}

/// Live allocations and bytes, summed over the slots.
pub fn live() -> (i64, i64) {
    TABLE.iter().fold((0, 0), |(allocs, bytes), slot| {
        (allocs + slot.allocs.load(Ordering::Relaxed), bytes + slot.bytes.load(Ordering::Relaxed))
    })
}

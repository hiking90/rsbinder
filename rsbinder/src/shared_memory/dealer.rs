// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `MemoryDealer` — AOSP's chunk allocator over one shared heap
//! (`frameworks/native/libs/binder/MemoryDealer.cpp`, Plan 4-7a Phase D).
//!
//! One heap is allocated and published **once**; every
//! [`allocate`](MemoryDealer::allocate) carves an `(offset, size)`
//! window out of it and hands it back as an [`Allocation`] — an
//! `android.utils.IMemory` whose `GET_MEMORY` reply names the shared
//! heap binder. A peer that keeps a [`HeapCache`](crate::shared_memory::HeapCache)
//! therefore maps the heap a single time and then resolves each
//! allocation to a bare offset: no fd passing and no `mmap` per buffer,
//! which is the whole point for streaming (camera / audio) traffic.
//!
//! Allocator = AOSP `SimpleBestFitAllocator`: 32-byte granules
//! (`kMemoryAlign`), best-fit over a sorted free list, and coalescing
//! with both neighbours on free. `PAGE_ALIGNED` allocations are
//! supported through [`allocate_page_aligned`](MemoryDealer::allocate_page_aligned).

use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::heap::MemoryHeapBase;
use super::wire::{export_heap, MemoryBase};
use super::{IMemory, IMemoryHeap};
use crate::binder::SIBinder;
use crate::error::{Result, StatusCode};

/// AOSP `SimpleBestFitAllocator::kMemoryAlign`: every allocation starts
/// on a 32-byte boundary and is a multiple of 32 bytes long.
pub const ALLOCATION_ALIGNMENT: usize = 32;

/// One run of granules; `free == false` means it is handed out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Chunk {
    start: usize, // in granules
    size: usize,  // in granules
    free: bool,
}

/// AOSP `SimpleBestFitAllocator` on a `Vec` kept sorted by `start`.
struct BestFit {
    chunks: Vec<Chunk>,
    granules_per_page: usize,
}

impl BestFit {
    fn new(heap_size: usize, page: usize) -> Self {
        Self {
            chunks: vec![Chunk {
                start: 0,
                size: heap_size / ALLOCATION_ALIGNMENT,
                free: true,
            }],
            granules_per_page: page / ALLOCATION_ALIGNMENT,
        }
    }

    /// Granules a chunk starting at `start` must skip to reach a page
    /// boundary (AOSP `-cur->start & (pagesize/kMemoryAlign - 1)`).
    fn page_pad(&self, start: usize) -> usize {
        start.wrapping_neg() & (self.granules_per_page - 1)
    }

    /// Returns the byte offset of the new block.
    fn alloc(&mut self, bytes: usize, page_aligned: bool) -> Option<usize> {
        let size = bytes.div_ceil(ALLOCATION_ALIGNMENT).max(1);
        let mut best: Option<usize> = None;
        for (i, c) in self.chunks.iter().enumerate() {
            let extra = if page_aligned {
                self.page_pad(c.start)
            } else {
                0
            };
            if c.free && c.size >= size + extra {
                if best.is_none_or(|b| c.size < self.chunks[b].size) {
                    best = Some(i);
                }
                if c.size == size {
                    break;
                }
            }
        }
        let i = best?;
        let Chunk {
            start,
            size: free_size,
            ..
        } = self.chunks[i];
        let extra = if page_aligned {
            self.page_pad(start)
        } else {
            0
        };
        // Split: [head pad (free)] [block (used)] [tail (free)].
        let mut replacement = Vec::with_capacity(3);
        if extra > 0 {
            replacement.push(Chunk {
                start,
                size: extra,
                free: true,
            });
        }
        replacement.push(Chunk {
            start: start + extra,
            size,
            free: false,
        });
        let tail = free_size - size - extra;
        if tail > 0 {
            replacement.push(Chunk {
                start: start + extra + size,
                size: tail,
                free: true,
            });
        }
        self.chunks.splice(i..=i, replacement);
        Some((start + extra) * ALLOCATION_ALIGNMENT)
    }

    /// `true` if a used block started at `offset` and is now free.
    fn dealloc(&mut self, offset: usize) -> bool {
        let start = offset / ALLOCATION_ALIGNMENT;
        let Some(i) = self.chunks.iter().position(|c| c.start == start) else {
            return false;
        };
        if self.chunks[i].free {
            return false;
        }
        self.chunks[i].free = true;
        // Coalesce with the free neighbour on each side.
        if i + 1 < self.chunks.len() && self.chunks[i + 1].free {
            self.chunks[i].size += self.chunks[i + 1].size;
            self.chunks.remove(i + 1);
        }
        if i > 0 && self.chunks[i - 1].free {
            self.chunks[i - 1].size += self.chunks[i].size;
            self.chunks.remove(i);
        }
        true
    }

    fn free_bytes(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| c.free)
            .map(|c| c.size * ALLOCATION_ALIGNMENT)
            .sum()
    }

    fn largest_free_bytes(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| c.free)
            .map(|c| c.size * ALLOCATION_ALIGNMENT)
            .max()
            .unwrap_or(0)
    }
}

/// Owner-side chunk allocator over one [`MemoryHeapBase`] (AOSP
/// `MemoryDealer`). Create it once, publish nothing — each
/// [`Allocation`] carries the heap binder inside its `GET_MEMORY` reply.
pub struct MemoryDealer {
    heap: Arc<MemoryHeapBase>,
    heap_binder: SIBinder,
    allocator: Mutex<BestFit>,
}

impl std::fmt::Debug for MemoryDealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryDealer")
            .field("heap_size", &self.heap.size())
            .field("free", &self.free_space())
            .finish()
    }
}

impl MemoryDealer {
    /// A dealer over a fresh `size`-byte (page-rounded) heap. `flags`
    /// are the [`MemoryHeapBase`] flags (e.g. `FLAG_READ_ONLY` makes
    /// every allocation read-only for peers while the owner may write).
    pub fn new(size: usize, flags: u32) -> Result<Arc<Self>> {
        Self::new_named(size, flags, "MemoryDealer")
    }

    /// [`new`](Self::new) with a debug name for the heap.
    pub fn new_named(size: usize, flags: u32, name: &str) -> Result<Arc<Self>> {
        let heap = Arc::new(MemoryHeapBase::new_named(size, flags, name)?);
        Self::over(heap)
    }

    /// A dealer over an existing, locally mapped heap (the heap must
    /// not be [`FLAG_DONT_MAP_LOCALLY`](super::FLAG_DONT_MAP_LOCALLY) —
    /// the owner reads and writes its allocations through the mapping).
    pub fn over(heap: Arc<MemoryHeapBase>) -> Result<Arc<Self>> {
        if heap.as_ptr().is_none() {
            return Err(StatusCode::InvalidOperation);
        }
        let page = rustix::param::page_size();
        let allocator = Mutex::new(BestFit::new(heap.size(), page));
        Ok(Arc::new(Self {
            heap_binder: export_heap(heap.clone()),
            heap,
            allocator,
        }))
    }

    /// Carve `size` bytes out of the heap (rounded up to
    /// [`ALLOCATION_ALIGNMENT`]). `NoMemory` when no free run is large
    /// enough — fragmentation counts, see [`largest_free_block`](Self::largest_free_block).
    pub fn allocate(self: &Arc<Self>, size: usize) -> Result<Allocation> {
        self.allocate_inner(size, false)
    }

    /// Like [`allocate`](Self::allocate) but the block starts on a page
    /// boundary (AOSP `MemoryDealer::PAGE_ALIGNED`).
    pub fn allocate_page_aligned(self: &Arc<Self>, size: usize) -> Result<Allocation> {
        self.allocate_inner(size, true)
    }

    fn allocate_inner(self: &Arc<Self>, size: usize, page_aligned: bool) -> Result<Allocation> {
        if size == 0 {
            return Err(StatusCode::BadValue);
        }
        let offset = self
            .allocator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .alloc(size, page_aligned)
            .ok_or(StatusCode::NoMemory)?;
        let rounded = size.div_ceil(ALLOCATION_ALIGNMENT) * ALLOCATION_ALIGNMENT;
        let mem = MemoryBase::new(
            self.heap.clone() as Arc<dyn IMemoryHeap>,
            self.heap_binder.clone(),
            offset,
            rounded,
        )?;
        Ok(Allocation {
            mem: Arc::new(mem),
            binder: OnceLock::new(),
            dealer: Arc::downgrade(self),
        })
    }

    fn deallocate(&self, offset: usize) {
        let mut a = self.allocator.lock().unwrap_or_else(|e| e.into_inner());
        if !a.dealloc(offset) {
            log::error!("MemoryDealer: block at offset {offset:#x} is not allocated");
        }
    }

    /// The shared heap.
    pub fn heap(&self) -> &Arc<MemoryHeapBase> {
        &self.heap
    }

    /// The `android.utils.IMemoryHeap` binder every allocation refers to.
    pub fn heap_binder(&self) -> &SIBinder {
        &self.heap_binder
    }

    /// Bytes currently free (possibly fragmented).
    pub fn free_space(&self) -> usize {
        self.allocator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .free_bytes()
    }

    /// Largest single allocation that would currently succeed.
    pub fn largest_free_block(&self) -> usize {
        self.allocator
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .largest_free_bytes()
    }
}

/// One block handed out by a [`MemoryDealer`] (AOSP `Allocation`): an
/// `IMemory` window plus RAII return of the block on drop. Publish it
/// with [`export`](Self::export); the owner reads and writes it through
/// [`read_at`](Self::read_at) / [`write_at`](Self::write_at).
pub struct Allocation {
    mem: Arc<MemoryBase>,
    binder: OnceLock<SIBinder>,
    dealer: Weak<MemoryDealer>,
}

impl std::fmt::Debug for Allocation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Allocation")
            .field("offset", &self.offset())
            .field("size", &self.size())
            .finish()
    }
}

impl Allocation {
    /// The `android.utils.IMemory` binder for this block (created once).
    pub fn export(&self) -> SIBinder {
        self.binder
            .get_or_init(|| self.mem.clone().export())
            .clone()
    }

    /// The window as a plain [`MemoryBase`].
    pub fn memory_base(&self) -> &Arc<MemoryBase> {
        &self.mem
    }

    /// Byte offset of the block within the dealer's heap.
    pub fn offset(&self) -> usize {
        self.mem.offset()
    }

    /// Byte length of the block (rounded up to [`ALLOCATION_ALIGNMENT`]).
    pub fn size(&self) -> usize {
        self.mem.size()
    }

    /// Copy out of the block (`off` is relative to the block).
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        let base = self.window(off, dst.len())?;
        self.dealer()?.heap.read_at(base, dst)
    }

    /// Copy into the block (`off` is relative to the block).
    pub fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        let base = self.window(off, src.len())?;
        self.dealer()?.heap.write_at(base, src)
    }

    fn dealer(&self) -> Result<Arc<MemoryDealer>> {
        self.dealer.upgrade().ok_or(StatusCode::DeadObject)
    }

    fn window(&self, off: usize, len: usize) -> Result<usize> {
        let end = off.checked_add(len).ok_or(StatusCode::BadValue)?;
        if end > self.mem.size() {
            return Err(StatusCode::BadValue);
        }
        Ok(self.mem.offset() + off)
    }
}

impl IMemory for Allocation {
    fn memory(&self) -> &dyn IMemoryHeap {
        self.mem.memory()
    }
    fn offset(&self) -> usize {
        Allocation::offset(self)
    }
    fn size(&self) -> usize {
        Allocation::size(self)
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        // Peers that still hold the exported binder keep reading the
        // window; only the dealer's bookkeeping changes (AOSP does the
        // same, optionally poisoning the bytes in debug builds).
        if let Some(d) = self.dealer.upgrade() {
            d.deallocate(self.mem.offset());
        }
    }
}

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]
mod tests {
    use super::*;

    fn page() -> usize {
        rustix::param::page_size()
    }

    #[test]
    fn best_fit_splits_and_coalesces() {
        let mut a = BestFit::new(1024, 4096);
        let x = a.alloc(100, false).unwrap(); // 4 granules
        let y = a.alloc(32, false).unwrap(); // 1 granule
        let z = a.alloc(200, false).unwrap(); // 7 granules
        assert_eq!((x, y, z), (0, 128, 160));
        assert_eq!(a.free_bytes(), 1024 - 384);
        // Free the middle one, then a 32-byte request must best-fit into it.
        assert!(a.dealloc(y));
        assert_eq!(a.alloc(1, false), Some(128));
        // Free everything in mixed order → single free chunk again.
        assert!(a.dealloc(x));
        assert!(a.dealloc(z));
        assert!(a.dealloc(128));
        assert_eq!(a.chunks.len(), 1);
        assert_eq!(a.free_bytes(), 1024);
        // Double free / unknown offset are rejected, not panics.
        assert!(!a.dealloc(128));
        assert!(!a.dealloc(7));
    }

    #[test]
    fn best_fit_prefers_smallest_sufficient_hole() {
        let mut a = BestFit::new(2048, 4096);
        let big = a.alloc(1024, false).unwrap();
        let _keep1 = a.alloc(64, false).unwrap();
        let small = a.alloc(64, false).unwrap();
        let _keep2 = a.alloc(64, false).unwrap();
        a.dealloc(big);
        a.dealloc(small);
        // Holes: [0,1024), [1088,1152) and the 832-byte tail. A 64-byte
        // request must land in the 64-byte hole, not a bigger one.
        assert_eq!(a.alloc(64, false), Some(1088));
    }

    #[test]
    fn page_aligned_allocation_pads_then_reuses_pad() {
        let mut a = BestFit::new(page() * 4, page());
        let first = a.alloc(64, false).unwrap();
        assert_eq!(first, 0);
        let aligned = a.alloc(100, true).unwrap();
        assert_eq!(aligned % page(), 0);
        assert_eq!(aligned, page());
        // The head pad [64, page) is free again for small blocks.
        assert_eq!(a.alloc(32, false), Some(64));
    }

    #[test]
    fn dealer_allocations_are_disjoint_windows_and_return_on_drop() {
        let d = MemoryDealer::new(page() * 2, 0).unwrap();
        let total = d.free_space();
        let a = d.allocate(100).unwrap();
        let b = d.allocate(page()).unwrap();
        assert_eq!(a.size(), 128);
        assert_eq!(b.size(), page());
        assert!(a.offset() + a.size() <= b.offset());
        assert_eq!(IMemory::memory(&a).size(), page() * 2);
        a.write_at(0, b"alpha").unwrap();
        b.write_at(0, b"beta").unwrap();
        let mut s = [0u8; 5];
        a.read_at(0, &mut s).unwrap();
        assert_eq!(&s, b"alpha");
        // Through the heap, the bytes sit at the allocation's offset.
        let mut h = [0u8; 4];
        d.heap().read_at(b.offset(), &mut h).unwrap();
        assert_eq!(&h, b"beta");
        assert_eq!(
            a.write_at(120, b"123456789").unwrap_err(),
            StatusCode::BadValue
        );
        assert_eq!(d.free_space(), total - 128 - page());
        drop(a);
        drop(b);
        assert_eq!(d.free_space(), total);
        assert_eq!(d.largest_free_block(), total);
    }

    #[test]
    fn dealer_exhaustion_is_no_memory_and_zero_is_bad_value() {
        let d = MemoryDealer::new(page(), 0).unwrap();
        let keep = d.allocate(page() - 32).unwrap();
        assert_eq!(d.allocate(64).unwrap_err(), StatusCode::NoMemory);
        assert_eq!(d.allocate(0).unwrap_err(), StatusCode::BadValue);
        let last = d.allocate(32).unwrap();
        assert_eq!(last.size(), 32);
        assert_eq!(d.free_space(), 0);
        drop(keep);
        assert_eq!(d.largest_free_block(), page() - 32);
        drop(last);
        assert_eq!(d.largest_free_block(), page());
    }

    #[test]
    fn allocation_outlives_dealer_gracefully() {
        let d = MemoryDealer::new(page(), 0).unwrap();
        let a = d.allocate(64).unwrap();
        let binder = a.export();
        assert!(binder == a.export(), "export() is stable");
        drop(d);
        // Heap stays alive through the MemoryBase; the dealer is gone.
        assert_eq!(
            a.read_at(0, &mut [0u8; 1]).unwrap_err(),
            StatusCode::DeadObject
        );
        assert_eq!(IMemory::memory(&a).size(), page());
        drop(a); // no panic without a dealer
    }

    #[test]
    fn dealer_refuses_unmapped_heap() {
        let heap =
            Arc::new(MemoryHeapBase::new(page(), super::super::FLAG_DONT_MAP_LOCALLY).unwrap());
        assert_eq!(
            MemoryDealer::over(heap).err(),
            Some(StatusCode::InvalidOperation)
        );
    }
}

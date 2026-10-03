// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Wire-faithful `android.utils.IMemoryHeap` / `android.utils.IMemory`
//! stubs (Plan 4-7a Phase B).
//!
//! AOSP `IMemory.cpp` is handwritten, not AIDL; the transaction layout
//! reproduced here is (android17-release):
//!
//! | interface | code | request | reply |
//! |---|---|---|---|
//! | `android.utils.IMemoryHeap` | `HEAP_ID` = `FIRST_CALL_TRANSACTION` | token only | **raw** `fd` object · `u64 size` · `i64 offset` · `u32 flags` (`IMemory.cpp:404-409`) |
//! | `android.utils.IMemory` | `GET_MEMORY` = `FIRST_CALL_TRANSACTION` | token only | `strong binder (heap)` · `i64 offset` · `u64 size` (`IMemory.cpp:239-245`) |
//!
//! Neither reply carries a `Status` header — the transaction status is
//! the only error channel, as in AOSP. The heap fd is AOSP
//! `writeFileDescriptor` / `readFileDescriptor`: the bare fd object with
//! **no** `ParcelFileDescriptor` not-null / comm markers (STAGE3 against
//! real libbinder is what pins this — rsbinder↔rsbinder is symmetric
//! and would not notice an 8-byte skew). The interface token on the
//! request is checked by the native dispatcher before `on_transact`
//! runs, so the `Bn*` types here do not re-check it.
//!
//! Proxies go through `SIBinder::as_remote` (kernel `ProxyHandle` or
//! RPC `RpcProxy`) and fall back to an in-process
//! `Transactable` call for a local binder, so one `Bp*` serves all
//! three cases. A `Bp*` resolves its remote geometry **once**, on
//! first use, like AOSP `BpMemoryHeap::assertReallyMapped()` /
//! `BpMemory::getMemory()`; it never re-transacts.
//!
//! # Tests
//!
//! - `heap_id_reply_layout_matches_aosp` parses the `HEAP_ID` reply
//!   (fd · u64 size · i64 offset · u32 flags) field by field rather than
//!   through `BpMemoryHeap`, so a field reorder fails it. It drives the
//!   stub directly because the native dispatcher that checks the token
//!   needs a kernel `ProcessState`; the proxy side is covered end-to-end
//!   over RPC in `tests/rpc_shared_memory.rs` and over the kernel in `tests/`.

use std::sync::{Arc, Mutex, OnceLock, Weak};

use super::heap::MappedHeap;
use super::{IMemory, IMemoryHeap};
use crate::binder::{Interface, Remotable, SIBinder, TransactionCode, FIRST_CALL_TRANSACTION};
use crate::error::{Result, StatusCode};
use crate::native::Binder;
use crate::parcel::Parcel;

/// AOSP `IMemoryHeap` interface descriptor (`IMemory.cpp:391`).
pub const IMEMORY_HEAP_DESCRIPTOR: &str = "android.utils.IMemoryHeap";
/// AOSP `IMemory` interface descriptor (`IMemory.cpp:226`).
pub const IMEMORY_DESCRIPTOR: &str = "android.utils.IMemory";

/// `BnMemoryHeap::HEAP_ID` (`IMemory.cpp:75-77`).
pub const HEAP_ID: TransactionCode = FIRST_CALL_TRANSACTION;
/// `BnMemory::GET_MEMORY` (`IMemory.cpp:123-125`).
pub const GET_MEMORY: TransactionCode = FIRST_CALL_TRANSACTION;

/// Token-only transaction: a remote binder via the proxy path, a local `Binder<T>` in-process.
fn call(binder: &SIBinder, descriptor: &str, code: TransactionCode) -> Result<Parcel> {
    if let Some(remote) = binder.as_remote() {
        // A known other interface is refused: the token must be `descriptor`, as AOSP writes it.
        let actual = binder.descriptor();
        if !actual.is_empty() && actual != descriptor {
            log::error!("shared memory: not an {descriptor}: {actual}");
            return Err(StatusCode::BadType);
        }
        // An unstamped RPC proxy stays unstamped; see `cancel_remote`.
        #[cfg(feature = "rpc")]
        if let Some(rp) = (**binder).as_any().downcast_ref::<crate::rpc::RpcProxy>() {
            let data = rp.build_request(descriptor)?;
            return rp
                .transact(code, &data, crate::FLAG_CLEAR_BUF)?
                .ok_or(StatusCode::UnexpectedNull);
        }
        let mut data = Parcel::new();
        data.write_interface_token(descriptor)?;
        return remote
            .submit_transact(code, &data, crate::FLAG_CLEAR_BUF)?
            .ok_or(StatusCode::UnexpectedNull);
    }
    let local = binder.as_transactable().ok_or(StatusCode::BadType)?;
    // A kernel-mode interface token needs the kernel `ProcessState`; RPC-only has none.
    if !crate::process_state::ProcessState::is_initialized() {
        return Err(StatusCode::InvalidOperation);
    }
    let mut data = Parcel::new();
    data.write_interface_token(descriptor)?;
    let mut reply = Parcel::new();
    local.transact(code, &mut data, &mut reply)?;
    reply.set_data_position(0);
    Ok(reply)
}

// --- IMemoryHeap ---

/// Server stub for a heap: `android.utils.IMemoryHeap` over any
/// [`IMemoryHeap`]. Wrap it with [`export_heap`] (or
/// `Binder::new(BnMemoryHeap(heap))`) to obtain a parcelable binder.
pub struct BnMemoryHeap<H: IMemoryHeap + 'static>(pub Arc<H>);

impl<H: IMemoryHeap + 'static> std::fmt::Debug for BnMemoryHeap<H> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BnMemoryHeap")
            .field("heap_id", &self.0.heap_id())
            .field("size", &self.0.size())
            .finish()
    }
}

impl<H: IMemoryHeap + 'static> Remotable for BnMemoryHeap<H> {
    fn descriptor() -> &'static str {
        IMEMORY_HEAP_DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            HEAP_ID => {
                let heap = &*self.0;
                let fd = heap.heap_fd().ok_or(StatusCode::BadValue)?;
                crate::file_descriptor::write_raw_fd(reply, fd)?;
                reply.write_u64(heap.size() as u64)?;
                reply.write_i64(heap.offset() as i64)?;
                reply.write_u32(heap.flags())
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Publish `heap` as an `android.utils.IMemoryHeap` binder.
pub fn export_heap<H: IMemoryHeap + 'static>(heap: Arc<H>) -> SIBinder {
    Interface::as_binder(&Binder::new(BnMemoryHeap(heap)))
}

/// Client stub for a remote `android.utils.IMemoryHeap`. Geometry and
/// the local mapping are fetched once on first use
/// ([`map`](Self::map)); until then the [`IMemoryHeap`] accessors
/// report a zero-sized, unmapped heap (AOSP `BpMemoryHeap` before
/// `assertReallyMapped()`).
pub struct BpMemoryHeap {
    binder: SIBinder,
    mapped: OnceLock<Arc<MappedHeap>>,
}

impl std::fmt::Debug for BpMemoryHeap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BpMemoryHeap")
            .field("mapped", &self.mapped.get())
            .finish()
    }
}

impl BpMemoryHeap {
    /// Wrap a binder obtained from a parcel / service lookup.
    pub fn new(binder: SIBinder) -> Self {
        Self {
            binder,
            mapped: OnceLock::new(),
        }
    }

    /// The underlying binder.
    pub fn as_binder(&self) -> &SIBinder {
        &self.binder
    }

    /// Fetch the heap geometry (`HEAP_ID`) and map it locally. The
    /// first call transacts; later calls return the cached mapping.
    /// Errors propagate from the transaction or from
    /// [`MappedHeap::from_parcel_fd`] (e.g. a peer claiming a size
    /// larger than the fd it sent).
    pub fn map(&self) -> Result<Arc<MappedHeap>> {
        if let Some(m) = self.mapped.get() {
            return Ok(m.clone());
        }
        let mut reply = call(&self.binder, IMEMORY_HEAP_DESCRIPTOR, HEAP_ID)?;
        let fd = crate::file_descriptor::read_raw_fd(&mut reply)?;
        let size64 = reply.read_u64()?;
        let offset64 = reply.read_i64()?;
        let flags = reply.read_u32()?;
        // AOSP ILP32 guard: the wire values must round-trip `usize`.
        let size = usize::try_from(size64).map_err(|_| StatusCode::BadValue)?;
        let offset = usize::try_from(offset64).map_err(|_| StatusCode::BadValue)?;
        let heap = Arc::new(MappedHeap::from_fd(fd, size, offset, flags)?);
        Ok(self.mapped.get_or_init(|| heap).clone())
    }

    /// The cached mapping, if [`map`](Self::map) has succeeded.
    pub fn mapped(&self) -> Option<&Arc<MappedHeap>> {
        self.mapped.get()
    }
}

impl IMemoryHeap for BpMemoryHeap {
    fn heap_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        self.mapped.get().and_then(|m| m.heap_fd())
    }
    fn size(&self) -> usize {
        self.mapped.get().map_or(0, |m| m.size())
    }
    fn flags(&self) -> u32 {
        self.mapped.get().map_or(0, |m| m.flags())
    }
    fn offset(&self) -> usize {
        self.mapped.get().map_or(0, |m| m.offset())
    }
    fn base(&self) -> Option<super::SharedBytes<'_>> {
        self.mapped.get().and_then(|m| m.base())
    }
}

/// Receiver-side cache of [`BpMemoryHeap`] per heap binder (AOSP
/// `HeapCache`). Allocations from one [`MemoryDealer`](super::MemoryDealer)
/// all name the same heap binder; resolving them through a shared cache
/// maps the heap **once** and turns every further allocation into a
/// bare `(offset, size)` — no fd passing, no `mmap` per buffer.
///
/// Entries are weak: the mapping lives as long as some [`BpMemory`] /
/// caller holds the `Arc<BpMemoryHeap>`; dead entries (and the heap
/// binder reference they pin) are dropped on the next access of any
/// kind. Keyed by binder identity (`SIBinder == SIBinder`), which is per
/// kernel handle / per RPC address.
#[derive(Default)]
pub struct HeapCache {
    entries: Mutex<Vec<(SIBinder, Weak<BpMemoryHeap>)>>,
}

impl std::fmt::Debug for HeapCache {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HeapCache")
            .field("live", &self.live().len())
            .finish()
    }
}

impl HeapCache {
    /// An empty cache, already in the `Arc` that
    /// [`BpMemory::new_with_cache`] takes so one instance is shared by
    /// every `IMemory` of a session.
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Lock the table with dead entries pruned; every accessor goes through here.
    fn live(&self) -> std::sync::MutexGuard<'_, Vec<(SIBinder, Weak<BpMemoryHeap>)>> {
        let mut entries = self.entries.lock().unwrap_or_else(|e| e.into_inner());
        entries.retain(|(_, w)| w.strong_count() > 0);
        entries
    }

    /// The shared proxy for `heap_binder`, creating it on first sight.
    pub fn get_or_insert(&self, heap_binder: &SIBinder) -> Arc<BpMemoryHeap> {
        let mut entries = self.live();
        if let Some(h) = entries
            .iter()
            .find(|(b, _)| b == heap_binder)
            .and_then(|(_, w)| w.upgrade())
        {
            return h;
        }
        let h = Arc::new(BpMemoryHeap::new(heap_binder.clone()));
        entries.push((heap_binder.clone(), Arc::downgrade(&h)));
        h
    }

    /// Number of heaps currently mapped through this cache.
    pub fn len(&self) -> usize {
        self.live().len()
    }

    /// `true` if no heap is currently held through this cache.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// --- IMemory ---

/// AOSP `MemoryBase`: an `(offset, size)` window onto a heap that is
/// already published as a binder (see [`export_heap`]).
pub struct MemoryBase {
    heap: Arc<dyn IMemoryHeap>,
    heap_binder: SIBinder,
    offset: usize,
    size: usize,
}

impl std::fmt::Debug for MemoryBase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MemoryBase")
            .field("heap_id", &self.heap.heap_id())
            .field("offset", &self.offset)
            .field("size", &self.size)
            .finish()
    }
}

impl MemoryBase {
    /// Window `[offset, offset + size)` of `heap`. `heap_binder` must be
    /// the binder serving that same heap; `BadValue` if the window does
    /// not fit inside `heap.size()`.
    pub fn new(
        heap: Arc<dyn IMemoryHeap>,
        heap_binder: SIBinder,
        offset: usize,
        size: usize,
    ) -> Result<Self> {
        check_window(heap.size(), offset, size)?;
        Ok(Self {
            heap,
            heap_binder,
            offset,
            size,
        })
    }

    /// Publish this window as an `android.utils.IMemory` binder.
    pub fn export(self: Arc<Self>) -> SIBinder {
        Interface::as_binder(&Binder::new(BnMemory(self)))
    }

    /// The binder serving the backing heap.
    pub fn heap_binder(&self) -> &SIBinder {
        &self.heap_binder
    }
}

impl IMemory for MemoryBase {
    fn memory(&self) -> &dyn IMemoryHeap {
        &*self.heap
    }
    fn offset(&self) -> usize {
        self.offset
    }
    fn size(&self) -> usize {
        self.size
    }
}

/// AOSP `BpMemory::getMemory` bounds rule (`IMemory.cpp:195-209`).
fn check_window(heap_size: usize, offset: usize, size: usize) -> Result<()> {
    if size <= heap_size && offset <= heap_size - size {
        Ok(())
    } else {
        Err(StatusCode::BadValue)
    }
}

/// Server stub: `android.utils.IMemory` over a [`MemoryBase`].
#[derive(Debug)]
pub struct BnMemory(pub Arc<MemoryBase>);

impl Remotable for BnMemory {
    fn descriptor() -> &'static str {
        IMEMORY_DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            GET_MEMORY => {
                reply.write(&self.0.heap_binder)?;
                reply.write_i64(self.0.offset as i64)?;
                reply.write_u64(self.0.size as u64)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Resolved `GET_MEMORY` reply.
struct Resolved {
    heap: Arc<BpMemoryHeap>,
    offset: usize,
    size: usize,
}

/// Client stub for a remote `android.utils.IMemory`. `GET_MEMORY` is
/// issued once on first use ([`resolve`](Self::resolve)); the heap it
/// names is mapped through [`BpMemoryHeap::map`].
///
/// A reply whose window does not fit the heap is clamped to
/// `offset = size = 0` (AOSP `IMemory.cpp:195-209`, bug 26877992), so a
/// misbehaving peer can never hand out an out-of-bounds window.
pub struct BpMemory {
    binder: SIBinder,
    resolved: OnceLock<Resolved>,
    cache: Option<Arc<HeapCache>>,
}

impl BpMemory {
    /// Wrap a binder obtained from a parcel / service lookup. The heap it
    /// names gets its own mapping; use [`new_with_cache`](Self::new_with_cache)
    /// when many `IMemory`s share one heap (a `MemoryDealer` peer).
    pub fn new(binder: SIBinder) -> Self {
        Self {
            binder,
            resolved: OnceLock::new(),
            cache: None,
        }
    }

    /// Like [`new`](Self::new), but the heap proxy is shared through
    /// `cache`, so allocations from the same dealer map the heap once.
    pub fn new_with_cache(binder: SIBinder, cache: Arc<HeapCache>) -> Self {
        Self {
            binder,
            resolved: OnceLock::new(),
            cache: Some(cache),
        }
    }

    /// The underlying binder.
    pub fn as_binder(&self) -> &SIBinder {
        &self.binder
    }

    /// Fetch and cache the `(heap, offset, size)` triple, mapping the
    /// heap. Returns the heap proxy; the window is then readable via
    /// [`IMemory::offset`] / [`IMemory::size`].
    pub fn resolve(&self) -> Result<Arc<BpMemoryHeap>> {
        if let Some(r) = self.resolved.get() {
            return Ok(r.heap.clone());
        }
        let mut reply = call(&self.binder, IMEMORY_DESCRIPTOR, GET_MEMORY)?;
        let heap_binder: SIBinder = reply.read()?;
        let offset64 = reply.read_i64()?;
        let size64 = reply.read_u64()?;
        let heap = match &self.cache {
            Some(c) => c.get_or_insert(&heap_binder),
            None => Arc::new(BpMemoryHeap::new(heap_binder)),
        };
        let mapped = heap.map()?;
        let (offset, size) = clamp_window(mapped.size(), offset64, size64);
        let r = self
            .resolved
            .get_or_init(|| Resolved { heap, offset, size });
        Ok(r.heap.clone())
    }

    /// Copy `dst.len()` bytes from window offset `off`.
    pub fn read_at(&self, off: usize, dst: &mut [u8]) -> Result<()> {
        let (heap, base) = self.window(off, dst.len())?;
        heap.read_at(base, dst)
    }

    /// Copy `src` to window offset `off`.
    pub fn write_at(&self, off: usize, src: &[u8]) -> Result<()> {
        let (heap, base) = self.window(off, src.len())?;
        heap.write_at(base, src)
    }

    fn window(&self, off: usize, len: usize) -> Result<(Arc<MappedHeap>, usize)> {
        let heap = self.resolve()?;
        let r = self.resolved.get().ok_or(StatusCode::Unknown)?;
        let end = off.checked_add(len).ok_or(StatusCode::BadValue)?;
        if end > r.size {
            return Err(StatusCode::BadValue);
        }
        let mapped = heap.map()?;
        Ok((mapped, r.offset + off))
    }
}

/// AOSP `BpMemory::getMemory` validation plus ILP32 round-trip; a failing window becomes `(0, 0)`.
fn clamp_window(heap_size: usize, offset64: i64, size64: u64) -> (usize, usize) {
    let ok = (|| {
        let size = usize::try_from(size64).ok()?;
        let offset = usize::try_from(offset64).ok()?;
        check_window(heap_size, offset, size).ok()?;
        Some((offset, size))
    })();
    ok.unwrap_or((0, 0))
}

impl IMemory for BpMemory {
    fn memory(&self) -> &dyn IMemoryHeap {
        match self.resolved.get() {
            Some(r) => &*r.heap,
            None => &UNRESOLVED,
        }
    }
    fn offset(&self) -> usize {
        self.resolved.get().map_or(0, |r| r.offset)
    }
    fn size(&self) -> usize {
        self.resolved.get().map_or(0, |r| r.size)
    }
}

/// Heap of an unresolved [`BpMemory`]; AOSP returns a null `sp<IMemoryHeap>`, the trait cannot.
struct UnresolvedHeap;
static UNRESOLVED: UnresolvedHeap = UnresolvedHeap;

impl IMemoryHeap for UnresolvedHeap {
    fn heap_fd(&self) -> Option<std::os::fd::BorrowedFd<'_>> {
        None
    }
    fn size(&self) -> usize {
        0
    }
    fn flags(&self) -> u32 {
        0
    }
    fn offset(&self) -> usize {
        0
    }
    fn base(&self) -> Option<super::SharedBytes<'_>> {
        None
    }
}

#[cfg(all(
    test,
    any(target_os = "linux", target_os = "android", target_os = "macos")
))]
mod tests {
    use super::*;
    use crate::shared_memory::{MemoryHeapBase, FLAG_READ_ONLY};

    fn page() -> usize {
        rustix::param::page_size()
    }

    /// `HEAP_ID` reply layout, parsed field by field; see module doc "Tests".
    #[test]
    fn heap_id_reply_layout_matches_aosp() {
        let heap = Arc::new(MemoryHeapBase::new(page() * 3, FLAG_READ_ONLY).unwrap());
        let bn = BnMemoryHeap(heap.clone());
        let mut reply = Parcel::new();
        bn.on_transact(HEAP_ID, &mut Parcel::new(), &mut reply)
            .unwrap();
        reply.set_data_position(0);
        // Bare fd object first — no ParcelFileDescriptor markers.
        let fd = crate::file_descriptor::read_raw_fd(&mut reply).unwrap();
        assert_eq!(reply.read_u64().unwrap(), (page() * 3) as u64);
        assert_eq!(reply.read_i64().unwrap(), 0);
        assert_eq!(reply.read_u32().unwrap(), FLAG_READ_ONLY);
        assert!(reply.read_u32().is_err(), "no trailing data");
        // The fd is a dup, not the heap's own fd, and maps the same pages.
        use std::os::fd::AsRawFd;
        assert_ne!(fd.as_raw_fd(), heap.heap_id());
        let rx = MappedHeap::from_fd(fd, page() * 3, 0, FLAG_READ_ONLY).unwrap();
        heap.write_at(page(), b"layout").unwrap();
        let mut b = [0u8; 6];
        rx.read_at(page(), &mut b).unwrap();
        assert_eq!(&b, b"layout");
    }

    /// `HEAP_ID` sends a dup of `heap_fd`, whatever `heap_id` says, and `BadValue` without one.
    #[test]
    fn heap_id_reply_is_built_from_heap_fd_only() {
        use std::os::fd::{AsFd, AsRawFd, BorrowedFd};

        struct Lying {
            heap: MemoryHeapBase,
            other: std::fs::File,
        }
        impl IMemoryHeap for Lying {
            fn heap_fd(&self) -> Option<BorrowedFd<'_>> {
                self.heap.heap_fd()
            }
            fn heap_id(&self) -> i32 {
                self.other.as_raw_fd()
            }
            fn size(&self) -> usize {
                self.heap.size()
            }
            fn flags(&self) -> u32 {
                0
            }
            fn offset(&self) -> usize {
                0
            }
            fn base(&self) -> Option<crate::shared_memory::SharedBytes<'_>> {
                None
            }
        }
        let ino = |fd: BorrowedFd<'_>| rustix::fs::fstat(fd).unwrap().st_ino;
        let lying = Arc::new(Lying {
            heap: MemoryHeapBase::new(page(), 0).unwrap(),
            other: std::fs::File::open("/dev/null").unwrap(),
        });
        let mut reply = Parcel::new();
        BnMemoryHeap(lying.clone())
            .on_transact(HEAP_ID, &mut Parcel::new(), &mut reply)
            .unwrap();
        reply.set_data_position(0);
        let fd = crate::file_descriptor::read_raw_fd(&mut reply).unwrap();
        assert_eq!(
            ino(fd.as_fd()),
            ino(lying.heap.as_fd()),
            "not the heap's fd"
        );

        let unmapped = BnMemoryHeap(Arc::new(BpMemoryHeap::new(export_heap(lying))));
        assert_eq!(
            unmapped
                .on_transact(HEAP_ID, &mut Parcel::new(), &mut Parcel::new())
                .unwrap_err(),
            StatusCode::BadValue
        );
    }

    #[test]
    fn unknown_code_is_rejected() {
        let bn = BnMemoryHeap(Arc::new(MemoryHeapBase::new(1, 0).unwrap()));
        assert_eq!(
            bn.on_transact(HEAP_ID + 1, &mut Parcel::new(), &mut Parcel::new())
                .unwrap_err(),
            StatusCode::UnknownTransaction
        );
        assert_eq!(
            <BnMemoryHeap<MemoryHeapBase> as Remotable>::descriptor(),
            IMEMORY_HEAP_DESCRIPTOR
        );
        assert_eq!(<BnMemory as Remotable>::descriptor(), IMEMORY_DESCRIPTOR);
        assert_eq!(
            export_heap(Arc::new(MemoryHeapBase::new(1, 0).unwrap())).descriptor(),
            IMEMORY_HEAP_DESCRIPTOR
        );
    }

    #[test]
    fn memory_base_window_is_bounds_checked() {
        let owner: Arc<dyn IMemoryHeap> = Arc::new(MemoryHeapBase::new(page(), 0).unwrap());
        let hb = export_heap(Arc::new(MemoryHeapBase::new(page(), 0).unwrap()));
        assert!(MemoryBase::new(owner.clone(), hb.clone(), 0, page()).is_ok());
        assert!(MemoryBase::new(owner.clone(), hb.clone(), page() - 8, 8).is_ok());
        assert_eq!(
            MemoryBase::new(owner.clone(), hb.clone(), page() - 7, 8).unwrap_err(),
            StatusCode::BadValue
        );
        assert_eq!(
            MemoryBase::new(owner, hb, 0, page() + 1).unwrap_err(),
            StatusCode::BadValue
        );
    }

    /// The four AOSP `IMemory.cpp:195-209` rejection branches collapse the window to `(0, 0)`.
    #[test]
    fn clamp_window_matches_aosp_rules() {
        assert_eq!(clamp_window(4096, 0, 4096), (0, 4096));
        assert_eq!(clamp_window(4096, 4000, 96), (4000, 96));
        assert_eq!(clamp_window(4096, 0, 4097), (0, 0)); // s > heap
        assert_eq!(clamp_window(4096, -1, 16), (0, 0)); // o < 0
        assert_eq!(clamp_window(4096, 4000, 97), (0, 0)); // o > heap - s
        assert_eq!(clamp_window(4096, 0, u64::MAX), (0, 0)); // ILP32-style overflow
    }

    /// A heap binder that is really another interface never runs that interface's code 1.
    #[cfg(feature = "rpc")]
    #[test]
    fn call_refuses_a_binder_of_another_interface() {
        use crate::rpc::transport::MemTransport;
        use crate::rpc::{AddressSpace, RpcSession};
        use std::sync::atomic::{AtomicBool, Ordering};

        struct Foo(Arc<AtomicBool>);
        impl Remotable for Foo {
            fn descriptor() -> &'static str {
                "x.y.IFoo"
            }
            fn on_transact(
                &self,
                code: TransactionCode,
                _: &mut Parcel,
                _: &mut Parcel,
            ) -> Result<()> {
                if code == FIRST_CALL_TRANSACTION {
                    self.0.store(true, Ordering::SeqCst);
                }
                Ok(())
            }
            fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
                Ok(())
            }
        }

        // Each case gets its own session, so its root proxy starts unstamped.
        let with_root = |case: &dyn Fn(SIBinder)| {
            let hit = Arc::new(AtomicBool::new(false));
            let (a, b) = MemTransport::pair();
            let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).unwrap();
            server
                .set_root(Interface::as_binder(&Binder::new(Foo(hit.clone()))))
                .unwrap();
            let handle = std::thread::spawn(move || {
                let _ = server.serve_blocking();
            });
            {
                let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).unwrap();
                case(client.get_root().unwrap());
            }
            handle.join().unwrap();
            assert!(!hit.load(Ordering::SeqCst), "x.y.IFoo code 1 ran");
        };
        // Stamped as another interface: refused before anything is sent.
        with_root(&|root| {
            crate::binder::__rpc_stamp_descriptor(&root, "x.y.IFoo");
            assert_eq!(
                BpMemoryHeap::new(root.clone()).map().unwrap_err(),
                StatusCode::BadType
            );
            assert_eq!(
                BpMemory::new(root).resolve().unwrap_err(),
                StatusCode::BadType
            );
        });
        // Unstamped: sent with the IMemoryHeap token, and the proxy is left unstamped.
        with_root(&|root| {
            assert!(BpMemoryHeap::new(root.clone()).map().is_err());
            assert_eq!(root.descriptor(), "");
        });
        // A real heap root would answer, so `BadType` here can only be the client's refusal.
        let (a, b) = MemTransport::pair();
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).unwrap();
        let heap = export_heap(Arc::new(MemoryHeapBase::new(page(), 0).unwrap()));
        server.set_root(heap).unwrap();
        let handle = std::thread::spawn(move || {
            let _ = server.serve_blocking();
        });
        {
            let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).unwrap();
            let root = client.get_root().unwrap();
            crate::binder::__rpc_stamp_descriptor(&root, "x.y.IFoo");
            assert_eq!(
                BpMemoryHeap::new(root).map().unwrap_err(),
                StatusCode::BadType
            );
        }
        handle.join().unwrap();
    }

    #[test]
    fn unresolved_bp_memory_reports_empty_window() {
        let bp = BpMemory::new(export_heap(Arc::new(MemoryHeapBase::new(1, 0).unwrap())));
        assert_eq!(bp.size(), 0);
        assert_eq!(bp.offset(), 0);
        assert_eq!(bp.memory().heap_id(), -1);
        assert!(bp.memory().base().is_none());
        // No kernel `ProcessState` (`InvalidOperation`) or token rejected (`BadType`); no panic.
        let err = bp.read_at(0, &mut [0u8; 1]).unwrap_err();
        assert!(
            matches!(err, StatusCode::InvalidOperation | StatusCode::BadType),
            "{err:?}"
        );
    }
}

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

use std::sync::{Arc, OnceLock};

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

/// Issue a token-only transaction to `binder` and return the reply.
/// Remote binders use the transport-generic proxy path; a local
/// `Binder<T>` is dispatched in-process.
fn call(binder: &SIBinder, descriptor: &str, code: TransactionCode) -> Result<Parcel> {
    if let Some(remote) = binder.as_remote() {
        // An RPC proxy writes the token from its stamped descriptor
        // (what generated `from_binder` does); a kernel proxy already
        // knows its descriptor from the driver.
        crate::binder::__rpc_stamp_descriptor(binder, descriptor);
        let data = remote.prepare_transact(true)?;
        return remote
            .submit_transact(code, &data, crate::FLAG_CLEAR_BUF)?
            .ok_or(StatusCode::UnexpectedNull);
    }
    let local = binder.as_transactable().ok_or(StatusCode::BadType)?;
    // A kernel-mode parcel's interface token goes through the
    // thread-state (strict-mode/work-source header), which needs the
    // kernel `ProcessState`; an RPC-only process has none.
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

// ---------------------------------------------------------------------
// IMemoryHeap
// ---------------------------------------------------------------------

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
                crate::file_descriptor::write_raw_fd(reply, borrow_heap_fd(heap.heap_id())?)?;
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

/// Borrow the heap's raw fd for the reply
/// (AOSP `reply->writeFileDescriptor(getHeapID())`, which dups).
fn borrow_heap_fd<'a>(raw: i32) -> Result<std::os::fd::BorrowedFd<'a>> {
    if raw < 0 {
        return Err(StatusCode::BadValue);
    }
    // SAFETY: `raw` is the live fd owned by the `IMemoryHeap` we are
    // serving, which outlives this call (it is behind `Arc` in the Bn),
    // and `write_raw_fd` only dups it.
    Ok(unsafe { std::os::fd::BorrowedFd::borrow_raw(raw) })
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
    fn heap_id(&self) -> i32 {
        self.mapped.get().map_or(-1, |m| m.heap_id())
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
    fn base(&self) -> Option<&[u8]> {
        self.mapped.get().and_then(|m| m.base())
    }
}

// ---------------------------------------------------------------------
// IMemory
// ---------------------------------------------------------------------

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

/// AOSP `BpMemory::getMemory` bounds rule (`IMemory.cpp:195-209`):
/// `size <= heap && offset <= heap - size`.
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
}

impl BpMemory {
    /// Wrap a binder obtained from a parcel / service lookup.
    pub fn new(binder: SIBinder) -> Self {
        Self {
            binder,
            resolved: OnceLock::new(),
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
        let heap = Arc::new(BpMemoryHeap::new(heap_binder));
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

/// AOSP `BpMemory::getMemory` validation, including the ILP32
/// round-trip check; a failing window becomes `(0, 0)`.
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

/// Placeholder heap reported by an unresolved [`BpMemory`] (AOSP
/// returns a null `sp<IMemoryHeap>`; the trait cannot).
struct UnresolvedHeap;
static UNRESOLVED: UnresolvedHeap = UnresolvedHeap;

impl IMemoryHeap for UnresolvedHeap {
    fn heap_id(&self) -> i32 {
        -1
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
    fn base(&self) -> Option<&[u8]> {
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

    /// `HEAP_ID` reply layout = fd · u64 size · i64 offset · u32 flags,
    /// parsed field-by-field rather than through `BpMemoryHeap` so a
    /// reordering would be caught. The stub is driven directly (the
    /// native dispatcher that checks the token needs a kernel
    /// `ProcessState`); the proxy side is covered end-to-end over RPC
    /// in `tests/rpc_fd.rs` and over the kernel in `tests/`.
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

    /// The four AOSP `IMemory.cpp:195-209` rejection branches collapse
    /// the window to `(0, 0)`.
    #[test]
    fn clamp_window_matches_aosp_rules() {
        assert_eq!(clamp_window(4096, 0, 4096), (0, 4096));
        assert_eq!(clamp_window(4096, 4000, 96), (4000, 96));
        assert_eq!(clamp_window(4096, 0, 4097), (0, 0)); // s > heap
        assert_eq!(clamp_window(4096, -1, 16), (0, 0)); // o < 0
        assert_eq!(clamp_window(4096, 4000, 97), (0, 0)); // o > heap - s
        assert_eq!(clamp_window(4096, 0, u64::MAX), (0, 0)); // ILP32-style overflow
    }

    #[test]
    fn unresolved_bp_memory_reports_empty_window() {
        let bp = BpMemory::new(export_heap(Arc::new(MemoryHeapBase::new(1, 0).unwrap())));
        assert_eq!(bp.size(), 0);
        assert_eq!(bp.offset(), 0);
        assert_eq!(bp.memory().heap_id(), -1);
        assert!(bp.memory().base().is_none());
        // Either the local dispatch path is unavailable (no kernel
        // `ProcessState`: `InvalidOperation`) or the heap binder rejects
        // `IMemory`'s token before dispatch (`BadType`). Never a panic.
        let err = bp.read_at(0, &mut [0u8; 1]).unwrap_err();
        assert!(
            matches!(err, StatusCode::InvalidOperation | StatusCode::BadType),
            "{err:?}"
        );
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The pieces a mapping is carved into, and the one place addresses of shared memory become
//! atomics.

use std::mem::{size_of, MaybeUninit};
use std::ops::{Deref, Range};
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicU32, AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;

use super::element::{bytes_of, bytes_of_mut, Element};
use super::mapping::Memory;
use super::{RoWord32, RoWord64};
use crate::error::{Error, Result};

const WORD: usize = size_of::<usize>();

#[cfg(target_has_atomic = "64")]
pub use std::sync::atomic::AtomicU64;

/// Stands in for std's `AtomicU64` on a target without 64-bit atomics: a
/// type with no value, so that code naming [`Word<AtomicU64>`](Word)
/// compiles on every target. [`Carve::word64`] and [`Carve::words64`]
/// return [`Error::Unsupported`] there, so no value of it exists and its
/// methods are never called.
#[cfg(not(target_has_atomic = "64"))]
#[derive(Debug)]
pub enum AtomicU64 {}

#[cfg(not(target_has_atomic = "64"))]
impl AtomicU64 {
    /// Never called: no value of this type exists.
    pub fn load(&self, _: Ordering) -> u64 {
        match *self {}
    }

    /// Never called: no value of this type exists.
    pub fn store(&self, _: u64, _: Ordering) {
        match *self {}
    }
}

mod sealed {
    pub trait Sealed {}
}

/// What a [`Word`] or [`Words`] views its bytes as: `AtomicU32` and
/// [`AtomicU64`] in a writable mapping, [`RoWord32`] and [`RoWord64`] in a
/// read-only one. Sealed.
pub trait WordType: sealed::Sealed {
    /// Bytes per word, and the alignment [`Carve`] asks of its address.
    #[doc(hidden)]
    const SIZE: usize;
}

macro_rules! word_types {
    ($($t:ty => $size:expr),* $(,)?) => { $(
        impl sealed::Sealed for $t {}
        impl WordType for $t {
            const SIZE: usize = $size;
        }
    )* };
}
word_types!(AtomicU32 => 4, AtomicU64 => 8, RoWord32 => 4, RoWord64 => 8);

/// One word of a carved mapping, keeping the mapping alive: it derefs to
/// the word's type, an `AtomicU32` or [`AtomicU64`] in a [`Carve`],
/// [`RoWord32`] or [`RoWord64`] in a [`RoCarve`].
///
/// The word's bytes belong to this piece alone, so every access this process
/// makes to them is an access of this size; a peer process's accesses are,
/// to Rust, another thread's atomics.
pub struct Word<T: WordType> {
    at: NonNull<T>,
    _memory: Arc<Memory>,
}

// SAFETY: a `&T` into memory `_memory` keeps alive; `T` is an atomic (or a type with no value).
unsafe impl<T: WordType + Sync> Send for Word<T> {}
// SAFETY: as for `Send`: `&Word` hands out `&T`, which `T: Sync` lets threads share.
unsafe impl<T: WordType + Sync> Sync for Word<T> {}

impl<T: WordType> Deref for Word<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &T {
        // SAFETY: aligned (`Carve::take`), inside `_memory`, which lives with `self`; atomic.
        unsafe { self.at.as_ref() }
    }
}

/// Another handle on the same word, at the same size.
impl<T: WordType> Clone for Word<T> {
    fn clone(&self) -> Self {
        Self {
            at: self.at,
            _memory: Arc::clone(&self._memory),
        }
    }
}

impl<T: WordType> std::fmt::Debug for Word<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Word").field("size", &T::SIZE).finish()
    }
}

impl Word<AtomicU32> {
    /// The same word, read-only: what a test that is both writer and reader
    /// of one heap memory gives its reader.
    pub fn as_ro(&self) -> Word<RoWord32> {
        Word {
            at: self.at.cast(),
            _memory: Arc::clone(&self._memory),
        }
    }
}

impl Word<AtomicU64> {
    /// The same word, read-only.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] where [`RoWord64::PRESENT`] is `false`.
    pub fn as_ro(&self) -> Result<Word<RoWord64>> {
        if !RoWord64::PRESENT {
            return Err(Error::Unsupported);
        }
        Ok(Word {
            at: self.at.cast(),
            _memory: Arc::clone(&self._memory),
        })
    }
}

/// `len` consecutive words of a carved mapping: [`Word`] for an array (topic
/// pin words, pool generations).
pub struct Words<T: WordType> {
    at: NonNull<T>,
    len: usize,
    _memory: Arc<Memory>,
}

// SAFETY: as for `Word`.
unsafe impl<T: WordType + Sync> Send for Words<T> {}
// SAFETY: as for `Word`.
unsafe impl<T: WordType + Sync> Sync for Words<T> {}

impl<T: WordType> Words<T> {
    /// Word `index`.
    ///
    /// # Panics
    ///
    /// When `index` is not below [`len`](Self::len).
    #[inline]
    pub fn get(&self, index: usize) -> &T {
        assert!(index < self.len, "word {index} of {}", self.len);
        // SAFETY: word `index` of the piece: aligned, inside `_memory`, which lives with `self`.
        unsafe { self.at.add(index).as_ref() }
    }

    /// The number of words.
    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether there is no word.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
}

/// Another handle on the same words, at the same size.
impl<T: WordType> Clone for Words<T> {
    fn clone(&self) -> Self {
        Self {
            at: self.at,
            len: self.len,
            _memory: Arc::clone(&self._memory),
        }
    }
}

impl<T: WordType> std::fmt::Debug for Words<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Words")
            .field("size", &T::SIZE)
            .field("len", &self.len)
            .finish()
    }
}

impl Words<AtomicU32> {
    /// The same words, read-only.
    pub fn as_ro(&self) -> Words<RoWord32> {
        Words {
            at: self.at.cast(),
            len: self.len,
            _memory: Arc::clone(&self._memory),
        }
    }
}

impl Words<AtomicU64> {
    /// The same words, read-only.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] where [`RoWord64::PRESENT`] is `false`.
    pub fn as_ro(&self) -> Result<Words<RoWord64>> {
        if !RoWord64::PRESENT {
            return Err(Error::Unsupported);
        }
        Ok(Words {
            at: self.at.cast(),
            len: self.len,
            _memory: Arc::clone(&self._memory),
        })
    }
}

/// A byte area of a carved mapping and the size each of its bytes is accessed at.
///
/// The split is fixed by the area: the bytes before its first `usize`-aligned address are
/// `AtomicU8`s (head), the whole `usize` words after them are `AtomicUsize`s, the bytes left are
/// `AtomicU8`s (tail). A copy that covers part of a word loads the whole word, or stores it with a
/// compare-exchange that writes the bytes around the copy back as it read them.
struct Area {
    at: NonNull<u8>,
    len: usize,
    head: usize,
    words: usize,
    _memory: Arc<Memory>,
}

/// One copy cut along the area's split.
struct Cut {
    head: Range<usize>,
    /// (word, first byte in it, bytes): the copy starts inside a word.
    lead: Option<(usize, usize, usize)>,
    /// (first word, words) the copy covers whole.
    whole: (usize, usize),
    /// (word, bytes from its start): the copy ends inside a word.
    trail: Option<(usize, usize)>,
    tail: Range<usize>,
}

impl Area {
    fn new(memory: Arc<Memory>, at: NonNull<u8>, len: usize) -> Self {
        let head = ((WORD - at.as_ptr() as usize % WORD) % WORD).min(len);
        let words = (len - head) / WORD;
        Self {
            at,
            len,
            head,
            words,
            _memory: memory,
        }
    }

    fn share(&self) -> Self {
        Self {
            at: self.at,
            len: self.len,
            head: self.head,
            words: self.words,
            _memory: Arc::clone(&self._memory),
        }
    }

    #[inline]
    fn bytes_at(&self, start: usize, n: usize) -> &[AtomicU8] {
        // SAFETY: `start..start + n` is head or tail (`cut`), inside the area and atomic-only.
        unsafe { std::slice::from_raw_parts(self.at.add(start).cast::<AtomicU8>().as_ptr(), n) }
    }

    #[inline]
    fn words_at(&self, first: usize, n: usize) -> &[AtomicUsize] {
        // Where an area has no whole word, the address past its head need not be aligned.
        if n == 0 {
            return &[];
        }
        let at = self
            .at
            .map_addr(|a| a.saturating_add(self.head + first * WORD));
        // SAFETY: whole aligned words of the area (`cut`), each only ever accessed as a word.
        unsafe { std::slice::from_raw_parts(at.cast::<AtomicUsize>().as_ptr(), n) }
    }

    #[inline]
    fn word(&self, index: usize) -> &AtomicUsize {
        &self.words_at(index, 1)[0]
    }

    /// Cut `off..off + len` along the split.
    ///
    /// # Panics
    ///
    /// When the range does not lie in the area.
    #[inline]
    fn cut(&self, off: usize, len: usize) -> Cut {
        let end = off.checked_add(len);
        assert!(
            end.is_some_and(|end| end <= self.len),
            "a copy of {len} bytes at {off} past an area of {}",
            self.len
        );
        let end = off + len;
        let word_end = self.head + self.words * WORD;
        let (ws, we) = (
            off.clamp(self.head, word_end),
            end.clamp(self.head, word_end),
        );
        let mut cut = Cut {
            head: off.min(self.head)..end.min(self.head),
            lead: None,
            whole: (0, 0),
            trail: None,
            tail: off.max(word_end)..end.max(word_end),
        };
        if ws < we {
            let mut at = ws;
            let shift = (ws - self.head) % WORD;
            if shift != 0 {
                let n = (WORD - shift).min(we - at);
                cut.lead = Some(((at - self.head) / WORD, shift, n));
                at += n;
            }
            let count = (we - at) / WORD;
            cut.whole = ((at - self.head) / WORD, count);
            at += count * WORD;
            if at < we {
                cut.trail = Some(((at - self.head) / WORD, we - at));
            }
        }
        cut
    }

    #[inline]
    fn merge(&self, index: usize, shift: usize, part: &[u8]) {
        let word = self.word(index);
        let mut seen = word.load(Ordering::Relaxed);
        loop {
            let mut bytes = seen.to_ne_bytes();
            bytes[shift..shift + part.len()].copy_from_slice(part);
            let next = usize::from_ne_bytes(bytes);
            match word.compare_exchange_weak(seen, next, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(now) => seen = now,
            }
        }
    }

    /// The words `off..off + len` is, when it is whole words of the word part: the common copy
    /// (a ring of 8-byte multiples), which then needs no cut. `None` otherwise.
    #[inline]
    fn whole_words(&self, off: usize, len: usize) -> Option<&[AtomicUsize]> {
        let at = off.checked_sub(self.head)?;
        let end = at.checked_add(len)?;
        if at % WORD != 0 || len % WORD != 0 || end > self.words * WORD {
            return None;
        }
        Some(self.words_at(at / WORD, len / WORD))
    }

    #[inline]
    fn store(&self, off: usize, src: &[u8]) {
        if let Some(words) = self.whole_words(off, src.len()) {
            for (d, s) in words.iter().zip(src.chunks_exact(WORD)) {
                d.store(
                    usize::from_ne_bytes(s.try_into().expect("a whole word")),
                    Ordering::Relaxed,
                );
            }
            return;
        }
        let cut = self.cut(off, src.len());
        let (head, mut src) = src.split_at(cut.head.len());
        for (d, s) in self.bytes_at(cut.head.start, head.len()).iter().zip(head) {
            d.store(*s, Ordering::Relaxed);
        }
        if let Some((index, shift, n)) = cut.lead {
            let (part, rest) = src.split_at(n);
            self.merge(index, shift, part);
            src = rest;
        }
        let (whole, rest) = src.split_at(cut.whole.1 * WORD);
        let words = self.words_at(cut.whole.0, cut.whole.1);
        for (d, s) in words.iter().zip(whole.chunks_exact(WORD)) {
            d.store(
                usize::from_ne_bytes(s.try_into().expect("a whole word")),
                Ordering::Relaxed,
            );
        }
        src = rest;
        if let Some((index, n)) = cut.trail {
            let (part, rest) = src.split_at(n);
            self.merge(index, 0, part);
            src = rest;
        }
        for (d, s) in self
            .bytes_at(cut.tail.start, cut.tail.len())
            .iter()
            .zip(src)
        {
            d.store(*s, Ordering::Relaxed);
        }
    }

    // Always `Relaxed`: the one load ordering defined on a read-only mapping.
    #[inline]
    fn load(&self, off: usize, dst: &mut [MaybeUninit<u8>]) {
        if let Some(words) = self.whole_words(off, dst.len()) {
            for (s, d) in words.iter().zip(dst.chunks_exact_mut(WORD)) {
                let d: &mut [MaybeUninit<u8>; WORD] = d.try_into().expect("a whole word");
                *d = s
                    .load(Ordering::Relaxed)
                    .to_ne_bytes()
                    .map(MaybeUninit::new);
            }
            return;
        }
        let cut = self.cut(off, dst.len());
        let (head, mut dst) = dst.split_at_mut(cut.head.len());
        for (s, d) in self.bytes_at(cut.head.start, head.len()).iter().zip(head) {
            *d = MaybeUninit::new(s.load(Ordering::Relaxed));
        }
        if let Some((index, shift, n)) = cut.lead {
            let (part, rest) = dst.split_at_mut(n);
            let bytes = self.word(index).load(Ordering::Relaxed).to_ne_bytes();
            for (d, s) in part.iter_mut().zip(&bytes[shift..shift + n]) {
                *d = MaybeUninit::new(*s);
            }
            dst = rest;
        }
        let (whole, rest) = dst.split_at_mut(cut.whole.1 * WORD);
        let words = self.words_at(cut.whole.0, cut.whole.1);
        for (s, d) in words.iter().zip(whole.chunks_exact_mut(WORD)) {
            let d: &mut [MaybeUninit<u8>; WORD] = d.try_into().expect("a whole word");
            *d = s
                .load(Ordering::Relaxed)
                .to_ne_bytes()
                .map(MaybeUninit::new);
        }
        dst = rest;
        if let Some((index, n)) = cut.trail {
            let (part, rest) = dst.split_at_mut(n);
            let bytes = self.word(index).load(Ordering::Relaxed).to_ne_bytes();
            for (d, s) in part.iter_mut().zip(&bytes[..n]) {
                *d = MaybeUninit::new(*s);
            }
            dst = rest;
        }
        for (s, d) in self
            .bytes_at(cut.tail.start, cut.tail.len())
            .iter()
            .zip(dst)
        {
            *d = MaybeUninit::new(s.load(Ordering::Relaxed));
        }
    }

    #[inline]
    fn load_into<T: Element>(&self, off: usize, dst: &mut [T]) {
        let dst = bytes_of_mut(dst);
        // SAFETY: `load` writes every byte of `dst` and only initialized values.
        let dst = unsafe { &mut *(std::ptr::from_mut(dst) as *mut [MaybeUninit<u8>]) };
        self.load(off, dst);
    }

    #[inline]
    fn load_extend(&self, off: usize, len: usize, dst: &mut Vec<u8>) {
        dst.reserve(len);
        let start = dst.len();
        self.load(off, &mut dst.spare_capacity_mut()[..len]);
        // SAFETY: `load` wrote the `len` bytes after `start` (it panics before writing otherwise).
        unsafe { dst.set_len(start + len) };
    }

    fn slice(&self, range: Range<usize>) -> NonNull<[u8]> {
        assert!(
            range.start <= range.end && range.end <= self.len,
            "bytes {range:?} past an area of {}",
            self.len
        );
        let at = self.at.map_addr(|a| a.saturating_add(range.start));
        NonNull::slice_from_raw_parts(at, range.end - range.start)
    }
}

// SAFETY: an address into memory `_memory` keeps alive, reached through atomics only.
unsafe impl Send for Area {}
// SAFETY: as for `Send`; every access through `&Area` is atomic, each byte at one size.
unsafe impl Sync for Area {}

/// A byte area of a [`Carve`]d mapping, copied in and out through atomics.
///
/// Memory another process can write must not be read through `&[u8]` or
/// written through `&mut [u8]`: the peer's store would race with a plain
/// access, which is undefined behavior however the result is used. `Bytes`
/// copies through atomics instead, and decides once, from the area's
/// address and length, the size each byte is accessed at: the bytes before
/// the first `usize` boundary and after the last whole `usize` word one at a
/// time, every other byte as part of its word. A copy that covers part of a
/// word loads the whole word, or stores it with a compare-exchange that
/// writes the other bytes of the word back unchanged. Every access to a
/// byte, by any copy from any thread, therefore has the same size, so two
/// threads may copy overlapping ranges at once: to Rust that is a race
/// between atomics, and the bytes read are some mix of old and new, never
/// undefined. A peer process's store is, to Rust, another thread's atomic
/// store as well.
///
/// [`store`](Self::store) and [`load`](Self::load) are `Relaxed`: they move
/// bytes and do not publish them. The protocol around them orders the copy
/// against whatever counter says the bytes are there (a `Release` store of
/// the counter after `store`, an `Acquire` load before `load`).
/// [`load_acquire`](Self::load_acquire) is for a protocol in which the bytes
/// themselves carry the order: one whose writer may overwrite bytes a reader
/// is copying, and whose reader finds out afterwards from a word the writer
/// stored before it started. The writer's side of it is a
/// `fence(Ordering::Release)` between that word and its `store`s.
///
/// Offsets and lengths are in bytes from the area's start, for elements of
/// any [`Element`] type. A range past the area's end is a panic: callers
/// check what a peer or a user gave them, and report it, before they copy.
pub struct Bytes(Area);

impl Bytes {
    /// The number of bytes of the area.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// Whether the area has no byte.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }

    /// Copy `src` to the bytes at `offset` with relaxed stores.
    ///
    /// # Panics
    ///
    /// When `offset + size_of_val(src)` exceeds [`len`](Self::len).
    #[inline]
    pub fn store<T: Element>(&self, offset: usize, src: &[T]) {
        self.0.store(offset, bytes_of(src));
    }

    /// Copy the bytes at `offset` to `dst` with relaxed loads.
    ///
    /// # Panics
    ///
    /// When `offset + size_of_val(dst)` exceeds [`len`](Self::len).
    #[inline]
    pub fn load<T: Element>(&self, offset: usize, dst: &mut [T]) {
        self.0.load_into(offset, dst);
    }

    /// Copy the bytes at `offset` to `dst`, and be ordered after every
    /// `fence(Ordering::Release)` that came before a [`store`](Self::store)
    /// whose bytes the copy read: what the storing thread did before that
    /// fence happens before this call returns. That holds for each byte on
    /// its own: a copy that overlaps the store may get some bytes of it and
    /// some older ones, in no order, and is ordered after the fence if it got
    /// at least one. The loads are `Relaxed` and one `fence(Ordering::Acquire)`
    /// follows them.
    ///
    /// # Panics
    ///
    /// As [`load`](Self::load).
    #[inline]
    pub fn load_acquire<T: Element>(&self, offset: usize, dst: &mut [T]) {
        self.0.load_into(offset, dst);
        fence(Ordering::Acquire);
    }

    /// Append the `len` bytes at `offset` to `dst` with relaxed loads, into
    /// the vector's spare capacity, which is not zeroed first. The vector
    /// reserves room first, as [`Vec::reserve`] does.
    ///
    /// # Panics
    ///
    /// When `offset + len` exceeds [`len`](Self::len), and when the new
    /// capacity would exceed `isize::MAX` bytes.
    #[inline]
    pub fn load_extend(&self, offset: usize, len: usize, dst: &mut Vec<u8>) {
        self.0.load_extend(offset, len, dst);
    }

    /// The same bytes, read-only, split the same way.
    pub fn as_ro(&self) -> RoBytes {
        RoBytes(self.0.share())
    }

    /// The address of the area's first byte. Using it is the caller's
    /// `unsafe`, and a plain access through it breaks what this type
    /// promises: it is for code that hands the address to C, and for tests
    /// that look the address up in `/proc/self/maps`.
    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        self.0.at.as_ptr()
    }
}

/// Another handle on the same area, split the same way.
impl Clone for Bytes {
    fn clone(&self) -> Self {
        Self(self.0.share())
    }
}

impl std::fmt::Debug for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bytes").field("len", &self.0.len).finish()
    }
}

/// A byte area of a [`RoCarve`]d mapping: [`Bytes`]' loads, and no store.
///
/// Its loads are `Relaxed` loads no wider than a `usize`, the kind the
/// standard library defines on read-only memory.
///
/// ```compile_fail,E0599
/// fn overwrite(bytes: &rsbinder_fmq::shm::RoBytes) {
///     bytes.store(0, &[0u8]);
/// }
/// ```
pub struct RoBytes(Area);

impl RoBytes {
    /// The number of bytes of the area.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// Whether the area has no byte.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.len == 0
    }

    /// Copy the bytes at `offset` to `dst` with relaxed loads: [`Bytes::load`].
    ///
    /// # Panics
    ///
    /// When `offset + size_of_val(dst)` exceeds [`len`](Self::len).
    #[inline]
    pub fn load<T: Element>(&self, offset: usize, dst: &mut [T]) {
        self.0.load_into(offset, dst);
    }

    /// [`Bytes::load_acquire`].
    ///
    /// # Panics
    ///
    /// As [`load`](Self::load).
    #[inline]
    pub fn load_acquire<T: Element>(&self, offset: usize, dst: &mut [T]) {
        self.0.load_into(offset, dst);
        fence(Ordering::Acquire);
    }

    /// [`Bytes::load_extend`].
    ///
    /// # Panics
    ///
    /// As [`Bytes::load_extend`].
    #[inline]
    pub fn load_extend(&self, offset: usize, len: usize, dst: &mut Vec<u8>) {
        self.0.load_extend(offset, len, dst);
    }

    /// The address of the area's first byte, as [`Bytes::as_ptr`].
    #[inline]
    pub fn as_ptr(&self) -> *const u8 {
        self.0.at.as_ptr()
    }
}

/// Another handle on the same area, split the same way.
impl Clone for RoBytes {
    fn clone(&self) -> Self {
        Self(self.0.share())
    }
}

impl std::fmt::Debug for RoBytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoBytes").field("len", &self.0.len).finish()
    }
}

/// The parts of a copy that wraps at the end of a ring of `capacity` bytes, for a ring kept
/// in a [`Bytes`] or a [`RoBytes`].
///
/// The copy is `len` bytes from ring byte `offset`. `part(at, range)` runs for the bytes
/// before the ring's end: ring byte `at`, bytes `range` of the copy. It runs again, with `at`
/// zero, for the bytes after the wrap, and not at all for that part when it is empty, which is
/// the common case. `offset` is a ring byte, already reduced below `capacity`, and `len` is
/// at most `capacity`, as the caller checked; a part past the area otherwise panics in the
/// copy method, as any copy past it does.
///
/// The parts are the two calls a copy method makes, of whichever order the protocol needs:
///
/// ```
/// # use rsbinder_fmq::shm::{ring_parts, Mapping};
/// let ring = Mapping::heap(64).unwrap().carve().bytes(0, 64).unwrap();
/// let src = [7u8; 16];
/// // 16 bytes from byte 56: 8 before the end, 8 from the start.
/// ring_parts(ring.len(), 56, src.len(), |at, range| ring.store(at, &src[range]));
/// let mut wrapped = [0u8; 8];
/// ring.load(0, &mut wrapped);
/// assert_eq!(wrapped, [7; 8]);
/// ```
#[inline]
pub fn ring_parts(
    capacity: usize,
    offset: usize,
    len: usize,
    mut part: impl FnMut(usize, Range<usize>),
) {
    let before = len.min(capacity.saturating_sub(offset));
    part(offset, 0..before);
    if before < len {
        part(0, before..len);
    }
}

/// A byte area whose bytes may also be read and written in place: [`Bytes`],
/// and slices through `unsafe`.
///
/// For a protocol that lends a region to one party at a time (a pool slot
/// that one publisher writes and its subscribers then read, plan 15), a copy
/// is the cost the slices avoid. Carving the region as `Lendable` is what
/// marks its bytes as ones a plain access may reach; no other piece has the
/// slices. The atomic copies of [`Bytes`] stay available through `Deref`.
pub struct Lendable(Bytes);

impl Deref for Lendable {
    type Target = Bytes;

    #[inline]
    fn deref(&self) -> &Bytes {
        &self.0
    }
}

impl Lendable {
    /// The bytes `range` of the area, in place.
    ///
    /// # Safety
    ///
    /// While the slice lives, no process writes those bytes, and this
    /// process makes no other access to them that writes: no
    /// [`slice_mut`](Self::slice_mut) over them, no store of `Bytes` that
    /// covers them, and no store of `Bytes` to a word that shares one of them
    /// (a store that ends or starts inside a `usize` word rewrites the
    /// whole word).
    ///
    /// # Panics
    ///
    /// When `range` does not lie in the area.
    #[inline]
    pub unsafe fn slice(&self, range: Range<usize>) -> &[u8] {
        // SAFETY: inside the area, which lives with `self`; the caller's contract leaves it unchanged.
        unsafe { self.0 .0.slice(range).as_ref() }
    }

    /// The bytes `range` of the area, in place, to write.
    ///
    /// # Safety
    ///
    /// While the slice lives, no process reads or writes those bytes but
    /// through it, and this process makes no other access to them, nor a
    /// store of `Bytes` to a word that shares one of them.
    ///
    /// # Panics
    ///
    /// When `range` does not lie in the area.
    #[inline]
    #[expect(
        clippy::mut_from_ref,
        reason = "the slots of one area are lent to several holders at once; the contract excludes overlap"
    )]
    pub unsafe fn slice_mut(&self, range: Range<usize>) -> &mut [u8] {
        // SAFETY: inside the area, which lives with `self`; the caller's contract makes it exclusive.
        unsafe { self.0 .0.slice(range).as_mut() }
    }

    /// The same bytes, read-only.
    pub fn as_ro(&self) -> RoLendable {
        RoLendable(self.0.as_ro())
    }
}

/// Another handle on the same area: one per holder a protocol lends parts of it to. The slices'
/// contract is on the bytes, not on the handle, so it covers every handle.
impl Clone for Lendable {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for Lendable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lendable")
            .field("len", &self.len())
            .finish()
    }
}

/// [`Lendable`] of a [`RoCarve`]d mapping: [`RoBytes`], and read-only slices
/// through `unsafe`.
pub struct RoLendable(RoBytes);

impl Deref for RoLendable {
    type Target = RoBytes;

    #[inline]
    fn deref(&self) -> &RoBytes {
        &self.0
    }
}

impl RoLendable {
    /// The bytes `range` of the area, in place.
    ///
    /// # Safety
    ///
    /// While the slice lives, no process writes those bytes. (This process
    /// cannot: the mapping is read-only.)
    ///
    /// # Panics
    ///
    /// When `range` does not lie in the area.
    #[inline]
    pub unsafe fn slice(&self, range: Range<usize>) -> &[u8] {
        // SAFETY: inside the area, which lives with `self`; the caller's contract leaves it unchanged.
        unsafe { self.0 .0.slice(range).as_ref() }
    }
}

/// Another handle on the same area, as for [`Lendable`].
impl Clone for RoLendable {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl std::fmt::Debug for RoLendable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoLendable")
            .field("len", &self.len())
            .finish()
    }
}

/// The ranges handed out so far, and what is left to hand out.
struct Pieces {
    memory: Arc<Memory>,
    at: NonNull<u8>,
    len: usize,
    taken: Vec<Range<usize>>,
}

// SAFETY: an address and the ranges handed out; `Pieces` itself never dereferences the address.
unsafe impl Send for Pieces {}
// SAFETY: as for `Send`; `&Pieces` gives out nothing.
unsafe impl Sync for Pieces {}

impl Pieces {
    fn new(memory: Memory, delta: usize) -> Self {
        let (at, len) = memory.at(delta);
        Self {
            memory: Arc::new(memory),
            at,
            len,
            taken: Vec::new(),
        }
    }

    /// The address of `offset..offset + len`, once it is checked and recorded.
    fn take(&mut self, offset: usize, len: usize, align: usize) -> Result<NonNull<u8>> {
        let end = offset
            .checked_add(len)
            .filter(|end| *end <= self.len)
            .ok_or(Error::BadValue("a piece past the end of its mapping"))?;
        let at = self.at.map_addr(|a| a.saturating_add(offset));
        if at.as_ptr() as usize % align != 0 {
            return Err(Error::BadValue("a word off its alignment"));
        }
        if len > 0 {
            if self.taken.iter().any(|r| r.start < end && offset < r.end) {
                return Err(Error::BadValue("two pieces overlap"));
            }
            self.taken.push(offset..end);
        }
        Ok(at)
    }

    fn word<T: WordType>(&mut self, offset: usize) -> Result<Word<T>> {
        let at = self.take(offset, T::SIZE, T::SIZE)?;
        Ok(Word {
            at: at.cast(),
            _memory: Arc::clone(&self.memory),
        })
    }

    fn words<T: WordType>(&mut self, offset: usize, len: usize) -> Result<Words<T>> {
        let bytes = len
            .checked_mul(T::SIZE)
            .ok_or(Error::BadValue("a piece past the end of its mapping"))?;
        let at = self.take(offset, bytes, T::SIZE)?;
        Ok(Words {
            at: at.cast(),
            len,
            _memory: Arc::clone(&self.memory),
        })
    }

    fn area(&mut self, offset: usize, len: usize) -> Result<Area> {
        let at = self.take(offset, len, 1)?;
        Ok(Area::new(Arc::clone(&self.memory), at, len))
    }
}

impl std::fmt::Debug for Pieces {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Pieces")
            .field("len", &self.len)
            .field("taken", &self.taken)
            .finish()
    }
}

/// A writable mapping, being split into the pieces its layout names
/// ([`Mapping::carve`](super::Mapping::carve)).
///
/// Each piece is a byte range at an offset from the byte the mapping was
/// made at. A word must be aligned to its size (the address, which for an
/// fd mapping is the offset given to `map` plus this offset). No two pieces
/// share a byte: every byte belongs to one piece, and the piece's kind
/// decides the size it is accessed at, so no byte is ever accessed at two
/// sizes through this mapping. Pieces keep the mapping alive; it is
/// unmapped when the last one is dropped.
///
/// A mapping is carved once: `carve` consumes it, and the pieces handed out
/// are all there are. Bytes no piece covers are never accessed.
///
/// Every method returns [`Error::BadValue`] for a piece that does not lie in
/// the mapping, a word off its alignment, or a piece that shares a byte with
/// one handed out before.
#[derive(Debug)]
pub struct Carve(Pieces);

impl Carve {
    pub(super) fn new(memory: Memory, delta: usize) -> Self {
        Self(Pieces::new(memory, delta))
    }

    /// The mapping's length: the bytes pieces can cover.
    #[expect(
        clippy::len_without_is_empty,
        reason = "a mapping is never empty, so `is_empty` would be a constant"
    )]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// The 4-byte word at `offset`.
    pub fn word32(&mut self, offset: usize) -> Result<Word<AtomicU32>> {
        self.0.word(offset)
    }

    /// The 8-byte word at `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] first on a target without 64-bit atomics.
    pub fn word64(&mut self, offset: usize) -> Result<Word<AtomicU64>> {
        if !cfg!(target_has_atomic = "64") {
            return Err(Error::Unsupported);
        }
        self.0.word(offset)
    }

    /// `len` 4-byte words from `offset`.
    pub fn words32(&mut self, offset: usize, len: usize) -> Result<Words<AtomicU32>> {
        self.0.words(offset, len)
    }

    /// `len` 8-byte words from `offset`.
    ///
    /// # Errors
    ///
    /// As [`word64`](Self::word64).
    pub fn words64(&mut self, offset: usize, len: usize) -> Result<Words<AtomicU64>> {
        if !cfg!(target_has_atomic = "64") {
            return Err(Error::Unsupported);
        }
        self.0.words(offset, len)
    }

    /// The `len` bytes at `offset`, copied through atomics.
    pub fn bytes(&mut self, offset: usize, len: usize) -> Result<Bytes> {
        Ok(Bytes(self.0.area(offset, len)?))
    }

    /// The `len` bytes at `offset`, copied through atomics or lent in place.
    pub fn lendable(&mut self, offset: usize, len: usize) -> Result<Lendable> {
        Ok(Lendable(self.bytes(offset, len)?))
    }
}

/// A read-only mapping, being split into the pieces its layout names
/// ([`RoMapping::carve`](super::RoMapping::carve)): [`Carve`], with pieces
/// that only load.
#[derive(Debug)]
pub struct RoCarve(Pieces);

impl RoCarve {
    pub(super) fn new(memory: Memory, delta: usize) -> Self {
        Self(Pieces::new(memory, delta))
    }

    /// The mapping's length: the bytes pieces can cover.
    #[expect(
        clippy::len_without_is_empty,
        reason = "a mapping is never empty, so `is_empty` would be a constant"
    )]
    pub fn len(&self) -> usize {
        self.0.len
    }

    /// The 4-byte word at `offset`.
    pub fn word32(&mut self, offset: usize) -> Result<Word<RoWord32>> {
        self.0.word(offset)
    }

    /// The 8-byte word at `offset`.
    ///
    /// # Errors
    ///
    /// [`Error::Unsupported`] first where [`RoWord64::PRESENT`] is `false`.
    pub fn word64(&mut self, offset: usize) -> Result<Word<RoWord64>> {
        if !RoWord64::PRESENT {
            return Err(Error::Unsupported);
        }
        self.0.word(offset)
    }

    /// `len` 4-byte words from `offset`.
    pub fn words32(&mut self, offset: usize, len: usize) -> Result<Words<RoWord32>> {
        self.0.words(offset, len)
    }

    /// `len` 8-byte words from `offset`.
    ///
    /// # Errors
    ///
    /// As [`word64`](Self::word64).
    pub fn words64(&mut self, offset: usize, len: usize) -> Result<Words<RoWord64>> {
        if !RoWord64::PRESENT {
            return Err(Error::Unsupported);
        }
        self.0.words(offset, len)
    }

    /// The `len` bytes at `offset`, loaded through atomics.
    pub fn bytes(&mut self, offset: usize, len: usize) -> Result<RoBytes> {
        Ok(RoBytes(self.0.area(offset, len)?))
    }

    /// The `len` bytes at `offset`, loaded through atomics or read in place.
    pub fn lendable(&mut self, offset: usize, len: usize) -> Result<RoLendable> {
        Ok(RoLendable(self.bytes(offset, len)?))
    }
}

#[cfg(test)]
mod tests;

#[cfg(all(test, loom))]
mod loom_tests;

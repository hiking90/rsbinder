// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::mem::{size_of, MaybeUninit};
use std::ptr::NonNull;
use std::sync::atomic::{fence, AtomicU8, AtomicUsize, Ordering};

const WORD: usize = size_of::<usize>();

/// A run of bytes of shared memory, copied in and out through atomics.
///
/// Memory another process can write must not be read through `&[u8]` or
/// written through `&mut [u8]`: the peer's store would race with a plain
/// access, which is undefined behavior however the result is used. A `Run`
/// views the bytes as atomics instead — bytes up to the first word boundary,
/// then whole words, then the remaining bytes — so a copy costs about what
/// `memcpy` costs, and a concurrent store by the peer is, to Rust, another
/// thread's atomic store: the bytes read may be a mix of old and new, never
/// undefined.
///
/// [`store`](Self::store) and [`load`](Self::load) are `Relaxed`: they move
/// bytes and do not publish them. The protocol around them orders the copy
/// against whatever counter says the bytes are there (a `Release` store of
/// the counter after `store`, an `Acquire` load before `load`). A counter in
/// an [`Access::ReadOnly`](super::Access::ReadOnly) mapping is read with a
/// `Relaxed` load followed by `fence(Ordering::Acquire)`, for the reason
/// given on that variant; [`RoWord32`](super::RoWord32) does so.
///
/// [`store_release`](Self::store_release) and
/// [`load_acquire`](Self::load_acquire) are for a protocol in which the
/// bytes themselves carry the order — one whose writer may overwrite bytes a
/// reader is copying, and whose reader finds out afterwards from a word the
/// writer stored *before* it started. There the reader's check needs "these
/// bytes are new, so that word is at least as new", which relaxed copies do
/// not give.
pub struct Run<'a> {
    head: &'a [AtomicU8],
    words: &'a [AtomicUsize],
    tail: &'a [AtomicU8],
}

impl Run<'_> {
    /// View `len` bytes at `at` as atomics.
    ///
    /// The memory need not be a [`Mapping`](super::Mapping); any allocation
    /// that satisfies the contract below does (a boxed slice of atomics in a
    /// test, for instance). A `len` of zero is allowed. `at` needs no
    /// alignment: the bytes below the first word boundary and past the last
    /// one are viewed a byte at a time.
    ///
    /// # Safety
    ///
    /// A `len` of zero puts no condition on `at`. For any other, during the
    /// whole lifetime `'a` the caller picks:
    ///
    /// * `len` is at most `isize::MAX`. A [`Mapping`](super::Mapping) is
    ///   never longer.
    /// * `at..at + len` lies within one allocation, which is not freed or
    ///   unmapped meanwhile. For a [`Mapping`](super::Mapping) that is one
    ///   `mmap` result; two mappings that happen to be adjacent are two
    ///   allocations.
    /// * `at` is [valid](std::ptr#safety) for reads of `len` bytes, and for
    ///   writes of them if [`store`](Self::store) or
    ///   [`store_release`](Self::store_release) is called on this run. Either
    ///   on a read-only mapping is a `SIGSEGV`; both are safe functions, so
    ///   the obligation is stated here.
    /// * Every byte of the range is initialized. The pages of a mapped file
    ///   or memfd are; memory from `alloc` or a `MaybeUninit` buffer is not
    ///   until it is written.
    /// * Every access to those bytes from this process is atomic: no `&[u8]`,
    ///   no `&mut`, no plain pointer read or write of them exists or is made
    ///   meanwhile.
    /// * No byte of the run is accessed at a different size at the same time
    ///   through addresses derived from the same allocation — for a
    ///   [`Mapping`](super::Mapping), the same `mmap` result. A byte is
    ///   accessed as part of a `usize` exactly when the whole `usize`-aligned
    ///   word around it lies inside `at..at + len`, and as a single byte
    ///   otherwise. Two overlapping runs used from two threads break this
    ///   wherever one covers such a word whole and the other only part of
    ///   it — at a different start, and equally at a different end; so does
    ///   an `AtomicU32` or `AtomicU64` placed over bytes a run covers. Keep
    ///   counters and other words outside the byte range that runs copy, and
    ///   let one thread at a time make overlapping runs.
    ///
    /// What another process does to the memory is not part of the contract.
    /// Nor is the size at which this process accesses it through a second
    /// `mmap` of the same object: those addresses are another allocation to
    /// Rust, and an atomic access through them is, like the peer's, another
    /// thread's atomic access.
    ///
    /// Nor is the length of a mapped file. An access to a page past its end —
    /// the range was mapped that far, or a holder of the fd truncated the
    /// file since — raises `SIGBUS`, whose default action ends the process;
    /// [`Mapping`](super::Mapping) says how to rule out both.
    #[inline]
    pub unsafe fn new(at: NonNull<u8>, len: usize) -> Self {
        let head_len = ((WORD - at.as_ptr() as usize % WORD) % WORD).min(len);
        let word_count = (len - head_len) / WORD;
        let tail_len = len - head_len - word_count * WORD;
        let at = at.as_ptr();
        // SAFETY: contract above; slices tile `at..at + len`, words aligned, empty is `&[]`.
        unsafe {
            let words: &[AtomicUsize] = if word_count == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(at.add(head_len).cast::<AtomicUsize>(), word_count)
            };
            Self {
                head: std::slice::from_raw_parts(at.cast::<AtomicU8>(), head_len),
                words,
                tail: std::slice::from_raw_parts(
                    at.add(head_len + word_count * WORD).cast::<AtomicU8>(),
                    tail_len,
                ),
            }
        }
    }

    /// The number of bytes the run covers.
    #[inline]
    pub fn len(&self) -> usize {
        self.head.len() + self.words.len() * WORD + self.tail.len()
    }

    /// Whether the run covers no byte.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copy `src` into the run with relaxed stores.
    ///
    /// # Panics
    ///
    /// When `src` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn store(&self, src: &[u8]) {
        self.store_with(src, Ordering::Relaxed);
    }

    /// Copy the run into `dst` with relaxed loads.
    ///
    /// # Panics
    ///
    /// When `dst` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn load(&self, dst: &mut [u8]) {
        self.load_with(dst);
    }

    /// Append the run to `dst` with relaxed loads: [`load`](Self::load) into
    /// the vector's spare capacity, which is not zeroed first.
    ///
    /// `dst`'s length grows by [`len`](Self::len) and what it held stays in
    /// front. The bytes past its old length are written once, by the loads,
    /// split as `load` splits them — bytes up to the first word boundary of
    /// the run, whole words, the remaining bytes — so a peer's concurrent
    /// store leaves a mix of old and new bytes, never undefined ones.
    ///
    /// The loads are `Relaxed` and no fence follows them, as for `load`. A
    /// caller that needs the order of [`load_acquire`](Self::load_acquire)
    /// follows the call with `fence(Ordering::Acquire)`, which is what
    /// `load_acquire` adds to `load`.
    ///
    /// The vector reserves room first, as [`Vec::reserve`] does: with less
    /// spare capacity than the run's length it reallocates, which moves the
    /// bytes it already holds. A caller that copies a long run into a buffer
    /// it reuses keeps the buffer's spare capacity — `capacity() - len()` —
    /// at least that length (`clear` it first, or start from
    /// [`Vec::with_capacity`]).
    ///
    /// # Panics
    ///
    /// When the new capacity would exceed `isize::MAX` bytes, as
    /// [`Vec::reserve`] does.
    #[inline]
    pub fn load_extend(&self, dst: &mut Vec<u8>) {
        let len = self.len();
        dst.reserve(len);
        let start = dst.len();
        let (head, rest) = dst.spare_capacity_mut()[..len].split_at_mut(self.head.len());
        let (words, tail) = rest.split_at_mut(self.words.len() * WORD);
        for (s, d) in self.head.iter().zip(head) {
            *d = MaybeUninit::new(s.load(Ordering::Relaxed));
        }
        for (s, d) in self.words.iter().zip(words.chunks_exact_mut(WORD)) {
            let word: &mut [MaybeUninit<u8>; WORD] = d.try_into().expect("a whole word");
            let bytes = s.load(Ordering::Relaxed).to_ne_bytes();
            *word = bytes.map(MaybeUninit::new);
        }
        for (s, d) in self.tail.iter().zip(tail) {
            *d = MaybeUninit::new(s.load(Ordering::Relaxed));
        }
        // SAFETY: the loops wrote every byte of `head`, `words`, `tail`: `len` bytes from `start`.
        unsafe { dst.set_len(start + len) };
    }

    /// Copy `src` into the run with `Release` stores.
    ///
    /// Whatever this thread did before the call — a store to a word that
    /// announces the write, say — happens before a
    /// [`load_acquire`](Self::load_acquire) that reads any byte this call
    /// stored. That holds for each byte on its own: a reader that overlaps
    /// the copy may get some bytes of this store and some older ones, and is
    /// ordered after this thread's earlier actions if it got at least one.
    ///
    /// On x86 this compiles to the code [`store`](Self::store) does. On
    /// AArch64 each word is an `STLR`, which costs more than a plain store.
    ///
    /// # Panics
    ///
    /// When `src` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn store_release(&self, src: &[u8]) {
        self.store_with(src, Ordering::Release);
    }

    /// Copy the run into `dst`, and be ordered after every
    /// [`store_release`](Self::store_release) whose bytes the copy read.
    ///
    /// After the call, a load this thread makes of a word sees a value at
    /// least as new as the one the storing thread saw or stored before its
    /// `store_release` began. A reader uses that to detect an overwrite: it
    /// copies, then loads the word the writer stores before it writes, and
    /// discards the copy when the word says the bytes were being replaced.
    ///
    /// The loads are `Relaxed` and one `fence(Ordering::Acquire)` follows
    /// them, which orders this thread as an `Acquire` load of each byte
    /// would. The copy is made this way so that it is defined through an
    /// [`Access::ReadOnly`](super::Access::ReadOnly) mapping, where the
    /// standard library promises nothing for a load of any other ordering.
    ///
    /// # Panics
    ///
    /// When `dst` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn load_acquire(&self, dst: &mut [u8]) {
        self.load_with(dst);
        fence(Ordering::Acquire);
    }

    #[inline]
    fn store_with(&self, src: &[u8], order: Ordering) {
        assert_eq!(
            src.len(),
            self.len(),
            "a run and its source differ in length"
        );
        let (head, rest) = src.split_at(self.head.len());
        let (words, tail) = rest.split_at(self.words.len() * WORD);
        for (d, s) in self.head.iter().zip(head) {
            d.store(*s, order);
        }
        for (d, s) in self.words.iter().zip(words.chunks_exact(WORD)) {
            let word: [u8; WORD] = s.try_into().expect("a whole word");
            d.store(usize::from_ne_bytes(word), order);
        }
        for (d, s) in self.tail.iter().zip(tail) {
            d.store(*s, order);
        }
    }

    // Always `Relaxed`: the one load ordering defined on a read-only mapping.
    #[inline]
    fn load_with(&self, dst: &mut [u8]) {
        assert_eq!(
            dst.len(),
            self.len(),
            "a run and its destination differ in length"
        );
        let (head, rest) = dst.split_at_mut(self.head.len());
        let (words, tail) = rest.split_at_mut(self.words.len() * WORD);
        for (s, d) in self.head.iter().zip(head) {
            *d = s.load(Ordering::Relaxed);
        }
        for (s, d) in self.words.iter().zip(words.chunks_exact_mut(WORD)) {
            let word: &mut [u8; WORD] = d.try_into().expect("a whole word");
            *word = s.load(Ordering::Relaxed).to_ne_bytes();
        }
        for (s, d) in self.tail.iter().zip(tail) {
            *d = s.load(Ordering::Relaxed);
        }
    }
}

impl std::fmt::Debug for Run<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Run").field("len", &self.len()).finish()
    }
}

/// A [`Run`] over bytes that this process reads and never writes: the
/// loads, and no store.
///
/// [`Run::store`] is a safe method, and on a run of an
/// [`Access::ReadOnly`](super::Access::ReadOnly) mapping it is a `SIGSEGV`.
/// Code that is handed an `RoRun` for a peer's memory cannot make that call:
///
/// ```compile_fail,E0599
/// fn overwrite(run: &rsbinder_fmq::shm::RoRun<'_>) {
///     run.store(&[]);
/// }
/// ```
///
/// Its loads are `Relaxed` loads no wider than a `usize`, the kind the
/// standard library defines on read-only memory.
pub struct RoRun<'a>(Run<'a>);

impl RoRun<'_> {
    /// View `len` bytes at `at` as atomics that are only loaded.
    ///
    /// # Safety
    ///
    /// The contract of [`Run::new`]; `at` need not be valid for writes.
    #[inline]
    pub unsafe fn new(at: NonNull<u8>, len: usize) -> Self {
        // SAFETY: the caller's contract is `Run::new`'s; no store is made through this value.
        Self(unsafe { Run::new(at, len) })
    }

    /// The number of bytes the run covers.
    #[inline]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether the run covers no byte.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Copy the run into `dst` with relaxed loads: [`Run::load`].
    ///
    /// # Panics
    ///
    /// When `dst` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn load(&self, dst: &mut [u8]) {
        self.0.load(dst);
    }

    /// Append the run to `dst` with relaxed loads, into spare capacity that
    /// is not zeroed first: [`Run::load_extend`].
    ///
    /// `dst`'s length grows by [`len`](Self::len); no fence follows the
    /// loads.
    ///
    /// # Panics
    ///
    /// When the new capacity would exceed `isize::MAX` bytes, as
    /// [`Vec::reserve`] does.
    #[inline]
    pub fn load_extend(&self, dst: &mut Vec<u8>) {
        self.0.load_extend(dst);
    }

    /// Copy the run into `dst`, and be ordered after every
    /// [`Run::store_release`] whose bytes the copy read:
    /// [`Run::load_acquire`].
    ///
    /// # Panics
    ///
    /// When `dst` is not exactly [`len`](Self::len) bytes long.
    #[inline]
    pub fn load_acquire(&self, dst: &mut [u8]) {
        self.0.load_acquire(dst);
    }
}

impl std::fmt::Debug for RoRun<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RoRun").field("len", &self.len()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buffer(words: usize) -> Box<[AtomicUsize]> {
        (0..words).map(|_| AtomicUsize::new(0)).collect()
    }

    fn run_at(buf: &[AtomicUsize], offset: usize, len: usize) -> Run<'_> {
        assert!(offset + len <= buf.len() * WORD);
        let at = NonNull::from(buf).cast::<u8>();
        // SAFETY: inside `buf`: initialized atomics, borrowed, atomic-only; threads share a split.
        unsafe { Run::new(NonNull::new_unchecked(at.as_ptr().add(offset)), len) }
    }

    // Under Miri run it with `-Zmiri-disable-weak-memory-emulation` (`scripts/local_gate.sh`).
    #[test]
    fn round_trip_at_every_start_and_length() {
        let buf = buffer(8);
        let all = buf.len() * WORD;
        for offset in 0..2 * WORD {
            for len in 0..=all - offset {
                let fill: Vec<u8> = vec![0xee; all];
                run_at(&buf, 0, all).store(&fill);
                let src: Vec<u8> = (0..len).map(|i| i as u8 ^ 0x5a).collect();
                run_at(&buf, offset, len).store(&src);
                let mut out = vec![0u8; len];
                run_at(&buf, offset, len).load(&mut out);
                assert_eq!(out, src, "offset {offset} len {len}");
                let mut extended = vec![0x11; 3];
                run_at(&buf, offset, len).load_extend(&mut extended);
                assert_eq!(extended[..3], [0x11; 3], "offset {offset} len {len}");
                assert_eq!(extended[3..], src, "offset {offset} len {len}");
                let mut whole = vec![0u8; all];
                run_at(&buf, 0, all).load(&mut whole);
                let outside = whole[..offset].iter().chain(&whole[offset + len..]);
                assert!(
                    outside.into_iter().all(|b| *b == 0xee),
                    "offset {offset} len {len}"
                );
            }
        }
    }

    /// Exists for Miri: a native run checks only byte values and passes even with a plain copy.
    #[test]
    fn racing_copies_are_atomic_accesses() {
        let rounds = if cfg!(miri) { 30 } else { 20_000 };
        let buf = buffer(4);
        // Starts mid-word: a head, one word and a tail, the shapes every copy is made of.
        let (offset, len) = (1, WORD + 9);
        std::thread::scope(|s| {
            let buf: &[AtomicUsize] = &buf;
            s.spawn(move || {
                for i in 0..rounds {
                    let byte = if i % 2 == 0 { 0xaa } else { 0x55 };
                    run_at(buf, offset, len).store(&vec![byte; len]);
                }
            });
            s.spawn(move || {
                let (mut out, mut ext) = (vec![0u8; len], Vec::new());
                for _ in 0..rounds {
                    run_at(buf, offset, len).load(&mut out);
                    assert!(out.iter().all(|b| [0, 0xaa, 0x55].contains(b)), "{out:x?}");
                    ext.clear();
                    run_at(buf, offset, len).load_extend(&mut ext);
                    assert_eq!(ext.len(), len);
                    assert!(ext.iter().all(|b| [0, 0xaa, 0x55].contains(b)), "{ext:x?}");
                }
            });
        });
    }

    #[test]
    fn an_empty_run_copies_nothing() {
        let buf = buffer(1);
        let run = run_at(&buf, 3, 0);
        assert!(run.is_empty());
        run.store(&[]);
        run.load(&mut []);
        run.store_release(&[]);
        run.load_acquire(&mut []);
        let (mut none, mut held) = (Vec::new(), vec![1, 2, 3]);
        run.load_extend(&mut none);
        run.load_extend(&mut held);
        assert_eq!((none, held), (vec![], vec![1, 2, 3]));
    }

    #[test]
    fn load_extend_appends_the_run_after_what_the_vector_holds() {
        let buf = buffer(8);
        // A head, whole words and a tail.
        let (offset, len) = (3, 2 * WORD + 7);
        let src: Vec<u8> = (0..len).map(|i| i as u8 ^ 0x3c).collect();
        run_at(&buf, offset, len).store(&src);
        let start = NonNull::from(&*buf).cast::<u8>();
        // SAFETY: the same bytes of `buf` as `run_at`'s, only loaded here.
        let reader = unsafe { RoRun::new(start.add(offset), len) };
        let held = [0x77u8; 5];
        // No spare capacity, as much as one run needs, more than two need.
        for spare in [0, len, 3 * len] {
            let mut extended = Vec::with_capacity(held.len() + spare);
            extended.extend_from_slice(&held);
            let capacity = extended.capacity();
            reader.load_extend(&mut extended);
            run_at(&buf, offset, len).load_extend(&mut extended);
            assert_eq!(extended.len(), held.len() + 2 * len, "spare {spare}");
            assert_eq!(extended[..held.len()], held, "spare {spare}");
            let (first, second) = extended[held.len()..].split_at(len);
            assert_eq!((first, second), (&src[..], &src[..]), "spare {spare}");
            if spare >= 2 * len {
                assert_eq!(extended.capacity(), capacity, "spare {spare}");
            }
        }
    }

    #[test]
    fn the_ordered_copies_move_the_same_bytes() {
        let buf = buffer(8);
        // A head, whole words and a tail.
        let (offset, len) = (3, 2 * WORD + 7);
        let src: Vec<u8> = (0..len).map(|i| i as u8 ^ 0xa5).collect();
        run_at(&buf, offset, len).store_release(&src);
        let mut out = vec![0u8; len];
        run_at(&buf, offset, len).load_acquire(&mut out);
        assert_eq!(out, src);
        run_at(&buf, offset, len).load(&mut out);
        assert_eq!(out, src);

        let start = NonNull::from(&*buf).cast::<u8>();
        // SAFETY: the same bytes of `buf` as `run_at`'s, only loaded here.
        let reader = unsafe { RoRun::new(start.add(offset), len) };
        assert_eq!((reader.len(), reader.is_empty()), (len, false));
        for load in [RoRun::load, RoRun::load_acquire] {
            out.fill(0);
            load(&reader, &mut out);
            assert_eq!(out, src);
        }
    }

    /// The order the two give. Natively x86 keeps it with relaxed copies too; Miri does not.
    #[test]
    fn a_byte_read_orders_the_reader_after_the_writers_announcement() {
        let (repeats, rounds) = if cfg!(miri) { (1, 30) } else { (200, 200u8) };
        // Starts mid-word: a head, one word and a tail.
        let (offset, len) = (1, WORD + 9);
        for _ in 0..repeats {
            let buf = buffer(4);
            let announced = AtomicUsize::new(0);
            std::thread::scope(|s| {
                let (buf, announced): (&[AtomicUsize], _) = (&buf, &announced);
                s.spawn(move || {
                    for round in 1..=rounds {
                        announced.store(usize::from(round), Ordering::Relaxed);
                        run_at(buf, offset, len).store_release(&vec![round; len]);
                    }
                });
                s.spawn(move || {
                    let mut out = vec![0u8; len];
                    for _ in 0..rounds {
                        run_at(buf, offset, len).load_acquire(&mut out);
                        let seen = announced.load(Ordering::Relaxed);
                        let newest = usize::from(*out.iter().max().unwrap());
                        assert!(seen >= newest, "read round {newest}, announced {seen}");
                    }
                });
            });
        }
    }
}

#[cfg(all(test, loom))]
mod loom_tests;

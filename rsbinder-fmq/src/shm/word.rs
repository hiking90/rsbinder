// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::num::NonZeroU32;
use std::sync::atomic::{fence, AtomicU32, Ordering};

use super::Deadline;
use crate::error::Result;

/// A 32-bit word of shared memory that this process reads and never writes:
/// what a [`Word`](super::Word) of a [`RoCarve`](super::RoCarve) derefs to.
///
/// A word in a [`RoMapping`](super::RoMapping) must not be handed around as
/// `&AtomicU32`: `store` and `fetch_add` are safe methods of that type, and
/// either one on a read-only page is a `SIGSEGV`. This type has the loads,
/// the futex wait and the futex wake, and nothing that stores, so code that
/// holds one cannot write the peer's memory:
///
/// ```compile_fail,E0599
/// fn overwrite(word: &rsbinder_fmq::shm::RoWord32) {
///     word.store(1, std::sync::atomic::Ordering::Relaxed);
/// }
/// ```
///
/// ```compile_fail,E0599
/// fn bump(word: &rsbinder_fmq::shm::RoWord32) {
///     word.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
/// }
/// ```
///
/// Every load is a `Relaxed` load of 4 bytes, the one kind the standard
/// library defines on read-only memory for every architecture it lists
/// (`core::sync::atomic`, "Atomic accesses to read-only memory").
#[repr(transparent)]
pub struct RoWord32(AtomicU32);

impl RoWord32 {
    /// The word's value now (a `Relaxed` load).
    #[inline]
    pub fn load(&self) -> u32 {
        self.0.load(Ordering::Relaxed)
    }

    /// The word's value, and be ordered after the `Release` store that wrote
    /// it: what the storing thread did before that store has happened before
    /// this call returns. A `Relaxed` load followed by
    /// `fence(Ordering::Acquire)`, which is defined on a read-only mapping
    /// where an `Acquire` load is not.
    #[inline]
    pub fn load_acquire(&self) -> u32 {
        let value = self.0.load(Ordering::Relaxed);
        fence(Ordering::Acquire);
        value
    }

    /// Sleep until a wake on this word whose mask intersects `bits`:
    /// [`futex_wait`](super::futex_wait) on the word, with its arguments,
    /// return value and errors. The kernel reads the word to compare it with
    /// `expected` and never writes it, so a read-only mapping is enough.
    #[inline]
    pub fn wait(&self, expected: u32, bits: NonZeroU32, deadline: Option<&Deadline>) -> Result<()> {
        super::futex_wait(&self.0, expected, bits, deadline)
    }

    /// Wake the waiters on this word whose mask intersects `bits`:
    /// [`futex_wake`](super::futex_wake) on the word, with its errors. A
    /// process that can only read the word can still wake those who sleep on
    /// it — the kernel asks for nothing more than a readable address — so a
    /// protocol must not take a wake as proof that the word changed.
    #[inline]
    pub fn wake(&self, bits: NonZeroU32) -> Result<()> {
        super::futex_wake(&self.0, bits)
    }
}

impl std::fmt::Debug for RoWord32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("RoWord32").field(&self.load()).finish()
    }
}

// std's 8-byte list, then x86 with SSE2 and ARM from v6; the `pub use` below repeats this.
#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "powerpc64",
    target_arch = "sparc64",
    target_arch = "s390x",
    all(target_arch = "x86", target_feature = "sse2"),
    all(target_arch = "arm", target_has_atomic = "64")
))]
mod wide {
    use super::*;

    /// A 64-bit word of shared memory that this process reads and never
    /// writes: [`RoWord32`] for a counter.
    ///
    /// The type exists where a `Relaxed` 8-byte load is known not to write,
    /// on two grounds:
    ///
    /// * The standard library defines that load on read-only memory for
    ///   `x86_64`, `aarch64`, `riscv64`, `loongarch64`, `mips64`,
    ///   `mips64r6`, `powerpc64`, `sparc64` and `s390x`.
    /// * On 32-bit `x86` with SSE2 and on 32-bit `arm` with 64-bit atomics
    ///   it promises nothing for a load that wide, which on a 32-bit
    ///   processor could be compiled as a compare-exchange, and that writes.
    ///   The ground there is the instruction LLVM generates for the load:
    ///   one `movq` on x86 and one `ldrexd` on ARM (ARMv6 and later),
    ///   neither of which writes. That is a property of the generated code,
    ///   not of the language, and the `cfg` of this type does not test
    ///   every option that decides it: with the x86 `soft-float` target
    ///   feature the load is a `lock cmpxchg8b`, which writes, while
    ///   `target_feature = "sse2"` holds. A test of this crate loads through
    ///   a read-only mapping and fails with `SIGSEGV` in a build whose load
    ///   writes.
    ///
    /// On every other target neither ground holds, and the name is a type
    /// with no value: [`PRESENT`](Self::PRESENT) is `false` there, and
    /// [`RoCarve::word64`](crate::shm::RoCarve::word64) returns
    /// [`Error::Unsupported`](crate::Error::Unsupported), so code that names
    /// `RoWord64` compiles on every target and refuses at run time. `x86`
    /// without SSE2 is among those targets: its load is `fild`/`fistp` with
    /// x87 alone and `movlps` with SSE, and no test of this crate runs
    /// either. There a 64-bit counter is read as an `AtomicU64` through a
    /// writable [`Mapping`](crate::shm::Mapping), on a target that has
    /// `AtomicU64` (`target_has_atomic = "64"`); a target without 64-bit
    /// atomics has no such type.
    ///
    /// ```compile_fail,E0599
    /// fn overwrite(word: &rsbinder_fmq::shm::RoWord64) {
    ///     word.store(1, std::sync::atomic::Ordering::Relaxed);
    /// }
    /// ```
    #[repr(transparent)]
    pub struct RoWord64(std::sync::atomic::AtomicU64);

    impl RoWord64 {
        /// Whether this target has the type: `true` here, `false` where the
        /// name is the type with no value. A caller that reads a peer's
        /// 64-bit counter through a read-only mapping returns its own
        /// "unsupported" error when this is `false`, before it maps.
        pub const PRESENT: bool = true;

        /// The word's value now (a `Relaxed` load).
        #[inline]
        pub fn load(&self) -> u64 {
            self.0.load(Ordering::Relaxed)
        }

        /// The word's value, and be ordered after the `Release` store that
        /// wrote it. See [`RoWord32::load_acquire`].
        #[inline]
        pub fn load_acquire(&self) -> u64 {
            let value = self.0.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            value
        }
    }

    impl std::fmt::Debug for RoWord64 {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_tuple("RoWord64").field(&self.load()).finish()
        }
    }

    #[cfg(test)]
    mod tests {
        use std::sync::atomic::Ordering;

        use crate::shm::Mapping;

        #[test]
        fn a_wide_word_reads_what_the_owner_stores() {
            let owner = Mapping::heap(8).unwrap().carve().word64(0).unwrap();
            let word = owner.as_ro().unwrap();
            owner.store(1 << 40, Ordering::Relaxed);
            assert_eq!(word.load(), 1 << 40);
            owner.store(u64::MAX, Ordering::Release);
            assert_eq!(word.load_acquire(), u64::MAX);
        }

        /// What the type's `cfg` rests on for 32-bit targets: a load that wrote would fault here.
        #[test]
        fn a_read_only_page_takes_the_wide_load() {
            let (mut writable, mut read_only) = crate::shm::word::tests::two_mappings();
            let (owner, word) = (writable.word64(0).unwrap(), read_only.word64(0).unwrap());
            // A load compiled as compare-exchange expects 0, and on ARM stores only on a match.
            assert_eq!(word.load(), 0);
            assert_eq!(word.load_acquire(), 0);
            owner.store(0x0123_4567_89ab_cdef, Ordering::Release);
            assert_eq!(word.load(), 0x0123_4567_89ab_cdef);
            assert_eq!(word.load_acquire(), 0x0123_4567_89ab_cdef);
        }
    }
}

#[cfg(any(
    target_arch = "x86_64",
    target_arch = "aarch64",
    target_arch = "riscv64",
    target_arch = "loongarch64",
    target_arch = "mips64",
    target_arch = "mips64r6",
    target_arch = "powerpc64",
    target_arch = "sparc64",
    target_arch = "s390x",
    all(target_arch = "x86", target_feature = "sse2"),
    all(target_arch = "arm", target_has_atomic = "64")
))]
pub use wide::RoWord64;

mod absent {
    /// A 64-bit word of shared memory that this process reads and never
    /// writes, on a target where no 8-byte load is known not to write: a
    /// type with no value.
    ///
    /// It has the methods of the type it stands in for, so that code naming
    /// `RoWord64` compiles on every target. [`PRESENT`](Self::PRESENT) is
    /// `false` and [`RoCarve::word64`](crate::shm::RoCarve::word64) refuses,
    /// so no reference to one exists and its loads are never called.
    #[allow(dead_code, reason = "named only where the real type is absent")]
    #[derive(Debug)]
    pub enum RoWord64 {}

    #[allow(dead_code, reason = "named only where the real type is absent")]
    impl RoWord64 {
        /// Whether this target has the type: `false` here.
        pub const PRESENT: bool = false;

        /// Never called: no value of this type exists.
        pub fn load(&self) -> u64 {
            match *self {}
        }

        /// Never called: no value of this type exists.
        pub fn load_acquire(&self) -> u64 {
            match *self {}
        }
    }
}

// A glob, which the `pub use` above shadows wherever the real type exists.
#[allow(unused_imports, reason = "shadowed on a target that has `RoWord64`")]
pub use absent::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shm::{Carve, Mapping, RoCarve, RoMapping};

    /// The first page of one unlinked file, mapped read-write and read-only, each carved.
    pub(super) fn two_mappings() -> (Carve, RoCarve) {
        use std::os::fd::AsFd;
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let page = rustix::param::page_size();
        let path = std::env::temp_dir().join(format!(
            "rsbinder-fmq-word-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::write(&path, vec![0u8; page]).unwrap();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let rw = Mapping::map(file.as_fd(), 0, page as u64).unwrap();
        let ro = RoMapping::map(file.as_fd(), 0, page as u64).unwrap();
        (rw.carve(), ro.carve())
    }

    #[test]
    fn a_read_only_page_takes_the_load() {
        let (mut writable, mut read_only) = two_mappings();
        let (owner, word) = (writable.word32(0).unwrap(), read_only.word32(0).unwrap());
        owner.store(0x89ab_cdef, Ordering::Release);
        assert_eq!(word.load(), 0x89ab_cdef);
        assert_eq!(word.load_acquire(), 0x89ab_cdef);
    }

    #[test]
    fn a_word_reads_what_the_owner_stores() {
        let owner = Mapping::heap(4).unwrap().carve().word32(0).unwrap();
        let word = owner.as_ro();
        owner.store(7, Ordering::Relaxed);
        assert_eq!(word.load(), 7);
        owner.store(9, Ordering::Release);
        assert_eq!(word.load_acquire(), 9);
        assert_eq!(format!("{:?}", *word), "RoWord32(9)");
    }

    #[test]
    fn the_stand_in_for_an_absent_wide_word_says_so() {
        const { assert!(!absent::RoWord64::PRESENT) };
    }

    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_wait_on_a_changed_word_returns_at_once() {
        let owner = Mapping::heap(4).unwrap().carve().word32(0).unwrap();
        owner.store(1, Ordering::Relaxed);
        let word = owner.as_ro();
        // The kernel compares first: `expected` is stale, so there is no sleep and no error.
        word.wait(0, NonZeroU32::MAX, None).unwrap();
        word.wake(NonZeroU32::MAX).unwrap();
    }
}

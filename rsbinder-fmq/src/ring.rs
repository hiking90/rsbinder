// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Copies in and out of ring memory, each access a relaxed atomic; see [`Regions`](crate::Regions).

use std::mem::size_of;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use crate::queue::Element;

const WORD: usize = size_of::<usize>();

/// A run of ring bytes as atomics: bytes up to a word boundary, words, then the rest.
pub(crate) struct Run<'a> {
    head: &'a [AtomicU8],
    words: &'a [AtomicUsize],
    tail: &'a [AtomicU8],
}

impl Run<'_> {
    /// Safety: `at..at + len` stays mapped, atomic-only, no concurrent other-size access.
    #[inline]
    pub(crate) unsafe fn new(at: NonNull<u8>, len: usize) -> Self {
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

    pub(crate) fn len(&self) -> usize {
        self.head.len() + self.words.len() * WORD + self.tail.len()
    }

    /// Relaxed stores of `src`, which is exactly as long as the run.
    #[inline]
    pub(crate) fn store(&self, src: &[u8]) {
        assert_eq!(
            src.len(),
            self.len(),
            "a run and its source differ in length"
        );
        let (head, rest) = src.split_at(self.head.len());
        let (words, tail) = rest.split_at(self.words.len() * WORD);
        for (d, s) in self.head.iter().zip(head) {
            d.store(*s, Ordering::Relaxed);
        }
        for (d, s) in self.words.iter().zip(words.chunks_exact(WORD)) {
            let word: [u8; WORD] = s.try_into().expect("a whole word");
            d.store(usize::from_ne_bytes(word), Ordering::Relaxed);
        }
        for (d, s) in self.tail.iter().zip(tail) {
            d.store(*s, Ordering::Relaxed);
        }
    }

    /// Relaxed loads into `dst`, which is exactly as long as the run.
    #[inline]
    pub(crate) fn load(&self, dst: &mut [u8]) {
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

/// The bytes of a caller's slice of elements (never ring memory).
pub(crate) fn bytes_of<T: Element>(user: &[T]) -> &[u8] {
    // SAFETY: `Element` has no padding, so every byte of `user` is initialized; `u8` aligns to 1.
    unsafe { std::slice::from_raw_parts(user.as_ptr().cast(), std::mem::size_of_val(user)) }
}

/// The bytes of a caller's slice of elements (never ring memory), to fill.
pub(crate) fn bytes_of_mut<T: Element>(user: &mut [T]) -> &mut [u8] {
    // SAFETY: as in `bytes_of`, and any bytes written leave a valid `T` (the `Element` contract).
    unsafe { std::slice::from_raw_parts_mut(user.as_mut_ptr().cast(), std::mem::size_of_val(user)) }
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
        // SAFETY: inside `buf`, which the test touches only through atomics.
        unsafe { Run::new(NonNull::new_unchecked(at.as_ptr().add(offset)), len) }
    }

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
                let mut out = vec![0u8; len];
                for _ in 0..rounds {
                    run_at(buf, offset, len).load(&mut out);
                    assert!(out.iter().all(|b| [0, 0xaa, 0x55].contains(b)), "{out:x?}");
                }
            });
        });
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use super::*;
use crate::shm::Mapping;

fn carve(len: usize) -> Carve {
    Mapping::heap(len).unwrap().carve()
}

#[test]
fn a_piece_must_lie_in_the_mapping_on_its_alignment_and_alone() {
    let mut c = carve(64);
    assert_eq!(c.len(), 64);
    let past = Error::BadValue("a piece past the end of its mapping");
    assert_eq!(c.bytes(60, 5).unwrap_err(), past);
    assert_eq!(c.bytes(usize::MAX, 2).unwrap_err(), past);
    assert_eq!(c.words32(0, usize::MAX).unwrap_err(), past);
    let unaligned = Error::BadValue("a word off its alignment");
    assert_eq!(c.word32(2).unwrap_err(), unaligned);
    let first = c.word32(0).unwrap();
    let overlap = Error::BadValue("two pieces overlap");
    assert_eq!(c.word32(0).unwrap_err(), overlap);
    assert_eq!(c.bytes(3, 2).unwrap_err(), overlap);
    // Neighbours that share no byte, and empty pieces anywhere.
    let ring = c.bytes(4, 20).unwrap();
    assert_eq!(c.bytes(10, 0).unwrap().len(), 0);
    assert!(c.words32(8, 0).unwrap().is_empty());
    let pins = c.words32(24, 2).unwrap();
    assert_eq!(c.bytes(28, 1).unwrap_err(), overlap);
    first.store(7, Relaxed);
    pins.get(1).store(9, Relaxed);
    ring.store(0, &[1u8; 20]);
    assert_eq!((first.load(Relaxed), pins.get(1).load(Relaxed)), (7, 9));
    #[cfg(target_has_atomic = "64")]
    {
        assert_eq!(c.word64(36).unwrap_err(), unaligned);
        c.word64(40).unwrap().store(u64::MAX, Relaxed);
        assert_eq!(c.words64(48, 2).unwrap().len(), 2);
    }
    #[cfg(not(target_has_atomic = "64"))]
    assert_eq!(c.word64(40).unwrap_err(), Error::Unsupported);
}

#[test]
fn a_heap_mapping_starts_zeroed_on_a_multiple_of_eight() {
    assert!(matches!(Mapping::heap(0), Err(Error::BadValue(_))));
    let mut c = carve(13);
    let bytes = c.bytes(0, 13).unwrap();
    assert_eq!(bytes.as_ptr() as usize % 8, 0);
    let mut out = [0xffu8; 13];
    bytes.load(0, &mut out);
    assert_eq!(out, [0; 13]);
}

#[test]
#[should_panic(expected = "word 2 of 2")]
fn a_word_past_the_array_panics() {
    let _ = carve(8).words32(0, 2).unwrap().get(2);
}

/// Every copy start and length, in areas that start at every byte of a word: heads, tails, both.
#[test]
fn copies_round_trip_at_every_start_and_length() {
    let span = if cfg!(miri) { 3 * WORD } else { 6 * WORD };
    for area_at in 0..WORD {
        let len = span - area_at - 3;
        let bytes = carve(span).bytes(area_at, len).unwrap();
        let reader = bytes.as_ro();
        for off in 0..=len.min(2 * WORD) {
            for n in 0..=len - off {
                bytes.store(0, &vec![0xeeu8; len]);
                let src: Vec<u8> = (0..n).map(|i| i as u8 ^ 0x5a).collect();
                bytes.store(off, &src);
                let mut whole = vec![0u8; len];
                reader.load(0, &mut whole);
                assert_eq!(whole[off..off + n], src, "area {area_at} copy {off}+{n}");
                let outside = whole[..off].iter().chain(&whole[off + n..]);
                assert!(outside.into_iter().all(|b| *b == 0xee), "area {area_at} copy {off}+{n}");
                let mut held = vec![0x11u8; 3];
                bytes.load_extend(off, n, &mut held);
                assert_eq!((&held[..3], &held[3..]), (&[0x11u8; 3][..], &src[..]));
            }
        }
    }
}

#[test]
fn elements_copy_as_their_bytes_and_the_ordered_copies_move_the_same_bytes() {
    let bytes = carve(64).bytes(4, 40).unwrap();
    let src = [0x0102_0304u32, 0xa0b0_c0d0, 7];
    bytes.store_release(4, &src);
    let mut out = [0u32; 3];
    bytes.load_acquire(4, &mut out);
    assert_eq!(out, src);
    let mut raw = [0u8; 12];
    bytes.as_ro().load_acquire(4, &mut raw);
    let expected: Vec<u8> = src.iter().flat_map(|w| w.to_ne_bytes()).collect();
    assert_eq!(raw[..], expected[..]);
}

#[test]
#[should_panic(expected = "past an area of 16")]
fn a_copy_past_the_area_panics() {
    carve(16).bytes(0, 16).unwrap().store(9, &[0u8; 8]);
}

#[test]
#[should_panic(expected = "past an area of 16")]
fn a_copy_whose_end_overflows_panics() {
    carve(16).bytes(0, 16).unwrap().load(usize::MAX, &mut [0u8; 2]);
}

#[test]
fn an_extend_past_the_area_leaves_the_vector_as_it_was() {
    let bytes = carve(16).bytes(0, 16).unwrap();
    let mut held = vec![1u8, 2];
    let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        bytes.load_extend(10, 7, &mut held);
    }));
    assert!(caught.is_err());
    assert_eq!(held, [1, 2]);
}

#[test]
fn words_read_back_through_their_read_only_views() {
    let mut c = carve(32);
    let word = c.word32(0).unwrap();
    let words = c.words32(4, 3).unwrap();
    word.store(5, Relaxed);
    words.get(2).store(6, Relaxed);
    assert_eq!((word.as_ro().load(), words.as_ro().get(2).load_acquire()), (5, 6));
    #[cfg(target_has_atomic = "64")]
    if RoWord64::PRESENT {
        let wide = c.word64(16).unwrap();
        let array = c.words64(24, 1).unwrap();
        wide.store(1 << 40, Relaxed);
        array.get(0).store(3, Relaxed);
        let (ro, ro_array) = (wide.as_ro().unwrap(), array.as_ro().unwrap());
        assert_eq!((ro.load(), ro_array.get(0).load()), (1 << 40, 3));
    }
}

#[test]
fn lendable_slices_see_what_the_copies_store_and_the_reverse() {
    let lent = carve(64).lendable(8, 48).unwrap();
    lent.store(3, &[9u8; 5]);
    // SAFETY: no other access to bytes 0..16 while the slices live; one thread.
    unsafe {
        assert_eq!(lent.slice(3..8), &[9u8; 5]);
        lent.slice_mut(16..24).copy_from_slice(&[4u8; 8]);
    }
    let mut out = [0u8; 8];
    lent.load(16, &mut out);
    assert_eq!(out, [4; 8]);
    // SAFETY: as above; the read-only view is another handle on the same bytes.
    assert_eq!(unsafe { lent.as_ro().slice(16..24) }, &[4u8; 8]);
}

#[test]
#[should_panic(expected = "past an area of 8")]
fn a_slice_past_the_area_panics() {
    // SAFETY: never formed: the range check panics first.
    let _ = unsafe { carve(8).lendable(0, 8).unwrap().slice(4..9) };
}

#[test]
fn pieces_go_to_other_threads_and_keep_the_memory() {
    fn is_send_sync<T: Send + Sync>() {}
    is_send_sync::<Word<AtomicU32>>();
    is_send_sync::<Word<RoWord32>>();
    is_send_sync::<Words<AtomicU32>>();
    is_send_sync::<Bytes>();
    is_send_sync::<RoBytes>();
    is_send_sync::<Lendable>();
    is_send_sync::<RoLendable>();
    is_send_sync::<Carve>();
    let bytes = {
        let mut c = carve(16);
        c.bytes(0, 16).unwrap()
    };
    std::thread::spawn(move || bytes.store(0, &[1u8; 16])).join().unwrap();
}

/// Exists for Miri: two threads copy overlapping bytes split at different places. With `Run`,
/// which splits by the copy's start, Miri reports a 1-byte store racing an 8-byte load.
#[test]
fn copies_at_different_starts_race_as_atomics() {
    let rounds = if cfg!(miri) { 12 } else { 5_000 };
    let bytes = carve(4 * WORD).bytes(0, 4 * WORD).unwrap();
    std::thread::scope(|s| {
        let bytes = &bytes;
        s.spawn(move || {
            for i in 0..rounds {
                bytes.store(1, &[(i % 2) as u8 * 0xaa; WORD + 9]);
            }
        });
        s.spawn(move || {
            let mut out = vec![0u8; 2 * WORD];
            for _ in 0..rounds {
                bytes.load(0, &mut out);
                assert!(out[1..].iter().all(|b| [0, 0xaa].contains(b)), "{out:x?}");
            }
        });
    });
}

/// A store that covers part of a word keeps what another thread stores to the rest of it.
#[test]
fn a_partial_store_keeps_its_neighbours() {
    let rounds = if cfg!(miri) { 20 } else { 20_000 };
    let bytes = carve(WORD).bytes(0, WORD).unwrap();
    std::thread::scope(|s| {
        let bytes = &bytes;
        s.spawn(move || {
            for i in 0..rounds {
                bytes.store(0, &[(i % 251) as u8 + 1; 2]);
            }
        });
        s.spawn(move || {
            for i in 0..rounds {
                let mine = [(i % 7) as u8 + 1; 2];
                bytes.store(2, &mine);
                let mut seen = [0u8; 2];
                bytes.load(2, &mut seen);
                assert_eq!(seen, mine, "round {i}");
            }
        });
    });
}

/// The order `store_release` and `load_acquire` give. Natively x86 keeps it with relaxed copies
/// too; Miri does not.
#[test]
fn a_byte_read_orders_the_reader_after_the_writers_announcement() {
    let (repeats, rounds) = if cfg!(miri) { (1, 30) } else { (200, 200u8) };
    // Starts mid-word: a lead, one whole word and a trail.
    let (offset, len) = (1, WORD + 9);
    for _ in 0..repeats {
        let bytes = carve(4 * WORD).bytes(0, 4 * WORD).unwrap();
        let announced = AtomicUsize::new(0);
        std::thread::scope(|s| {
            let (bytes, announced) = (&bytes, &announced);
            s.spawn(move || {
                for round in 1..=rounds {
                    announced.store(usize::from(round), Relaxed);
                    bytes.store_release(offset, &vec![round; len]);
                }
            });
            s.spawn(move || {
                let mut out = vec![0u8; len];
                for _ in 0..rounds {
                    bytes.load_acquire(offset, &mut out);
                    let seen = announced.load(Relaxed);
                    let newest = usize::from(out.iter().copied().max().unwrap_or(0));
                    assert!(seen >= newest, "read round {newest}, announced {seen}");
                }
            });
        });
    }
}

/// A reader's view of a writer's area: what it loads is some mix of old and new bytes.
#[test]
fn a_read_only_view_races_its_writer_as_atomics() {
    let rounds = if cfg!(miri) { 12 } else { 5_000 };
    let bytes = carve(3 * WORD).bytes(3, 2 * WORD + 1).unwrap();
    let reader = bytes.as_ro();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let (bytes, done) = (&bytes, &done);
        s.spawn(move || {
            for i in 0..rounds {
                bytes.store(2, &[if i % 2 == 0 { 0x33u8 } else { 0x44 }; WORD]);
            }
            done.store(true, Relaxed);
        });
        let mut out = Vec::new();
        while !done.load(Relaxed) {
            out.clear();
            reader.load_extend(0, 2 * WORD + 1, &mut out);
            assert!(out[2..2 + WORD].iter().all(|b| [0, 0x33, 0x44].contains(b)), "{out:x?}");
        }
    });
}

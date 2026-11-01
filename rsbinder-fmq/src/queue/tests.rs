// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `Regions` over heap memory, which is what Miri can run (it has no memfd and no `mmap`).
//!
//! Every copy `MessageQueue` and the unsynchronized writer make goes through
//! [`Regions::write_at`] or [`Regions::read_at`]. Under Miri a plain read or
//! write of ring memory anywhere on that path races the peer thread's atomic
//! stores and is reported as a data race; outside `shm` no such access can be
//! written without `unsafe`, which the crate denies.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::Relaxed};

use super::*;

/// Ring bytes: four words on a 64-bit target, eight on a 32-bit one.
const RING: usize = 32;
/// The ring's bytes before either thread writes; no read after this thread's own write returns it.
const FILL: u8 = 0xee;
const OURS: u8 = 0x33;
const PEER: [u8; 2] = [0xaa, 0x55];

/// Exists for Miri: natively the bytes come out right even through a plain copy.
#[test]
fn regions_copy_atomically() {
    let rounds = if cfg!(miri) { 20 } else { 2_000 };
    // (position, n, start, len): two wrapped byte runs, a run inside one word, a run over a word.
    let shapes = [
        (27u64, 9, 0, 9),
        (27, 9, 2, 6),
        (20, 5, 1, 4),
        (20, 12, 0, 12),
    ];
    for (position, n, start, len) in shapes {
        let shape = (position, n, start, len);
        let ring = Mapping::heap(RING).unwrap().carve().bytes(0, RING).unwrap();
        ring.store(0, &[FILL; RING]);
        // Relaxed only: an `Acquire` load of `stored` would order the copy after the peer's store.
        let (stored, done) = (AtomicUsize::new(0), AtomicBool::new(false));
        std::thread::scope(|s| {
            let (ring, stored, done) = (&ring, &stored, &done);
            // Each thread makes its own `Regions` over the one ring, at the same split.
            let regions = move |release: bool| Regions::<u8>::over(ring, position, n, release);
            // The peer breaks the protocol: it keeps storing into the elements this thread copies.
            let peer = s.spawn(move || {
                let mut i = 0;
                while !done.load(Relaxed) {
                    let src = vec![PEER[i % 2]; len];
                    regions(i % 2 == 0)
                        .write_at(start, &src)
                        .expect("peer write");
                    stored.fetch_add(1, Relaxed);
                    i += 1;
                }
            });
            s.spawn(move || {
                // Set on unwind too: `scope` joins the peer before it passes a panic on.
                struct Stop<'a>(&'a AtomicBool);
                impl Drop for Stop<'_> {
                    fn drop(&mut self) {
                        self.0.store(true, Relaxed);
                    }
                }
                let _stop = Stop(done);
                let (src, mut out) = (vec![OURS; len], vec![0u8; len]);
                for round in 0..rounds {
                    regions(false).write_at(start, &src).expect("write");
                    // The pause is right before the copy: the peer stores between write and read.
                    let seen = stored.load(Relaxed);
                    while stored.load(Relaxed) == seen && !peer.is_finished() {
                        std::thread::yield_now();
                    }
                    regions(false).read_at(start, &mut out).expect("read");
                    let whole = out.iter().all(|b| *b == OURS || PEER.contains(b));
                    assert!(whole, "{shape:?} round {round}: {out:x?}");
                }
            });
        });
        let mut bytes = [0u8; RING];
        ring.load(0, &mut bytes);
        let offset = (position % RING as u64) as usize;
        let split: Vec<usize> = (start..start + len).map(|e| (offset + e) % RING).collect();
        for (at, b) in bytes.iter().enumerate() {
            if split.contains(&at) {
                assert!(*b == OURS || PEER.contains(b), "{shape:?} byte {at}: {b:x}");
            } else {
                assert_eq!(*b, FILL, "{shape:?} byte {at} is outside the copies");
            }
        }
    }
}

/// A span past the ring is a panic in the copy, not an access outside the mapping.
#[test]
#[should_panic(expected = "past an area of 32")]
fn regions_past_the_ring_panic_at_the_copy() {
    let ring = Mapping::heap(RING).unwrap().carve().bytes(0, RING).unwrap();
    // 45 one-byte elements from byte 24: 8 before the wrap, 37 after it, more than the ring.
    let regions = Regions::<u8>::over(&ring, 24, 45, false);
    let mut out = [0u8; 45];
    let _ = regions.read_at(0, &mut out);
}

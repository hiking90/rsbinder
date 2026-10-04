// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One process, one queue, two handles (the creator's and an attached one).
//! Plan 12-fmq AC-12.1 (same process), 12.2, 12.3, 12.4, 12.5, 12.6.

#![cfg(any(target_os = "linux", target_os = "android"))]

use std::os::fd::{AsFd, OwnedFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rsbinder_fmq::{
    AttachPolicy, Descriptor, Error, Flavor, Grantor, MessageQueue, NOT_EMPTY, NOT_FULL,
};

fn policy(max_capacity: usize, event_flag: bool) -> AttachPolicy {
    AttachPolicy {
        max_capacity,
        require_seal: true,
        require_event_flag: event_flag,
    }
}

fn attach<T: rsbinder_fmq::Element>(q: &MessageQueue<T>) -> MessageQueue<T> {
    let desc = q.descriptor().expect("descriptor");
    MessageQueue::<T>::attach(&desc, &policy(q.capacity(), desc.has_event_flag())).expect("attach")
}

fn bad(r: Result<MessageQueue<u32>, Error>) -> &'static str {
    match r {
        Err(Error::BadValue(what)) => what,
        Err(e) => panic!("expected BadValue, got {e:?}"),
        Ok(_) => panic!("expected BadValue, attach succeeded"),
    }
}

/// A raw view of a single-fd queue's counters, to play the hostile peer.
struct Counters {
    base: *mut u8,
    len: usize,
}

impl Counters {
    fn map(desc: &Descriptor) -> Self {
        let len = rsbinder_fmq::shm::region_size(desc.fds[0].as_fd()).unwrap() as usize;
        // SAFETY: a fresh shared mapping of the whole fd; unmapped in Drop.
        let base = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                rustix::mm::ProtFlags::READ | rustix::mm::ProtFlags::WRITE,
                rustix::mm::MapFlags::SHARED,
                &desc.fds[0],
                0,
            )
        }
        .unwrap()
        .cast::<u8>();
        Self { base, len }
    }

    fn counter(&self, index: usize) -> &AtomicU64 {
        // SAFETY: offsets 0 and 8 of the default layout, inside the mapping.
        unsafe { &*self.base.add(index * 8).cast::<AtomicU64>() }
    }

    fn set(&self, read: u64, write: u64) {
        self.counter(0).store(read, Ordering::SeqCst);
        self.counter(1).store(write, Ordering::SeqCst);
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        // SAFETY: exactly what `mmap` returned.
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.len) };
    }
}

// ---- AC-12.1: round trip in one process --------------------------------

#[test]
fn create_descriptor_attach_round_trip() {
    let mut writer = MessageQueue::<u32>::create(16, true).unwrap();
    let desc = writer.descriptor().unwrap();
    assert_eq!(desc.fds.len(), 1);
    assert_eq!(desc.grantors.len(), 4);
    assert_eq!(desc.quantum, 4);
    assert_eq!(desc.flavor, Flavor::SynchronizedReadWrite);
    assert_eq!(desc.capacity(), Some(16));
    assert_eq!(desc.grantors[2].extent, 64);

    let mut reader = MessageQueue::<u32>::attach(&desc, &policy(16, true)).unwrap();
    assert_eq!(reader.capacity(), 16);
    assert_eq!(reader.quantum(), 4);
    assert_eq!(writer.available_to_write().unwrap(), 16);
    assert_eq!(reader.available_to_read().unwrap(), 0);

    let items: Vec<u32> = (1..=10).collect();
    assert!(writer.write(&items).unwrap());
    assert_eq!(reader.available_to_read().unwrap(), 10);
    assert_eq!(writer.available_to_write().unwrap(), 6);

    let mut out = [0u32; 10];
    assert!(reader.read(&mut out).unwrap());
    assert_eq!(&out[..], &items[..]);
    assert_eq!(reader.available_to_read().unwrap(), 0);
    assert_eq!(writer.available_to_write().unwrap(), 16);
}

#[test]
fn descriptor_fds_are_duplicates() {
    let q = MessageQueue::<u8>::create(8, false).unwrap();
    let a = q.descriptor().unwrap();
    let b = q.descriptor().unwrap();
    use std::os::fd::AsRawFd;
    assert_ne!(a.fds[0].as_raw_fd(), b.fds[0].as_raw_fd());
    drop(a);
    // The queue still works after a descriptor's fds are closed.
    let mut r = MessageQueue::<u8>::attach(&b, &policy(8, false)).unwrap();
    let mut w = q;
    assert!(w.write(&[7]).unwrap());
    let mut out = [0u8];
    assert!(r.read(&mut out).unwrap());
    assert_eq!(out, [7]);
}

#[test]
fn queue_without_event_flag_has_three_grantors_and_no_blocking() {
    let mut q = MessageQueue::<u8>::create(8, false).unwrap();
    let desc = q.descriptor().unwrap();
    assert_eq!(desc.grantors.len(), 3);
    assert!(!desc.has_event_flag());
    assert!(q.event_flag().is_none());
    assert_eq!(
        q.write_blocking(&[1], NOT_FULL, NOT_EMPTY, None)
            .unwrap_err(),
        Error::NoEventFlag
    );
    let mut out = [0u8];
    assert_eq!(
        q.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, None)
            .unwrap_err(),
        Error::NoEventFlag
    );
}

// ---- AC-12.2: wrap ------------------------------------------------------

#[test]
fn wrap_splits_the_regions_and_keeps_the_order() {
    let mut w = MessageQueue::<u16>::create(8, false).unwrap();
    let mut r = attach(&w);

    assert!(w.write(&[1, 2, 3, 4, 5]).unwrap());
    let mut out = [0u16; 5];
    assert!(r.read(&mut out).unwrap());

    // Position 5 of 8: 3 before the end, 3 after the wrap.
    {
        let mut regions = w.begin_write(6).unwrap().expect("6 fit");
        assert_eq!(regions.len(), 6);
        assert_eq!(regions.first_len(), 3);
        assert_eq!(regions.second_len(), 3);
        regions.write_at(0, &[10, 11]).unwrap();
        regions.write_at(2, &[12, 13, 14, 15]).unwrap();
        assert_eq!(
            regions.write_at(5, &[0, 0]).unwrap_err(),
            Error::BadValue("write past the reserved elements")
        );
    }
    w.commit_write(6).unwrap();

    {
        let regions = r.begin_read(6).unwrap().expect("6 available");
        assert_eq!(regions.first_len(), 3);
        assert_eq!(regions.second_len(), 3);
        let mut a = [0u16; 2];
        let mut b = [0u16; 4];
        regions.read_at(0, &mut a).unwrap();
        regions.read_at(2, &mut b).unwrap();
        assert_eq!(a, [10, 11]);
        assert_eq!(b, [12, 13, 14, 15]);
        // Starting at the wrap and past it: only the second part is read.
        let mut at_wrap = [0u16; 3];
        let mut past_wrap = [0u16; 2];
        regions.read_at(3, &mut at_wrap).unwrap();
        regions.read_at(4, &mut past_wrap).unwrap();
        assert_eq!(at_wrap, [13, 14, 15]);
        assert_eq!(past_wrap, [14, 15]);
        let mut c = [0u16; 1];
        assert_eq!(
            regions.read_at(6, &mut c).unwrap_err(),
            Error::BadValue("read past the available elements")
        );
    }
    r.commit_read(6).unwrap();
    assert_eq!(r.available_to_read().unwrap(), 0);

    // Many laps: the counters keep growing and every element comes back.
    let mut next = 0u16;
    for lap in 0..50u16 {
        let n = (lap % 8 + 1) as usize;
        let items: Vec<u16> = (0..n as u16).map(|i| next + i).collect();
        assert!(w.write(&items).unwrap());
        let mut got = vec![0u16; n];
        assert!(r.read(&mut got).unwrap());
        assert_eq!(got, items);
        next += n as u16;
    }
}

#[test]
fn write_and_read_are_all_or_nothing() {
    let mut w = MessageQueue::<u8>::create(4, false).unwrap();
    let mut r = attach(&w);
    assert!(!w.write(&[0; 5]).unwrap());
    assert!(w.begin_write(5).unwrap().is_none());
    assert!(w.write(&[1, 2, 3, 4]).unwrap());
    assert!(!w.write(&[5]).unwrap());
    assert!(w.write(&[]).unwrap());
    let mut five = [0u8; 5];
    assert!(!r.read(&mut five).unwrap());
    let mut two = [0u8; 2];
    assert!(r.read(&mut two).unwrap());
    assert_eq!(two, [1, 2]);
    assert!(w.write(&[5, 6]).unwrap());
    let mut four = [0u8; 4];
    assert!(r.read(&mut four).unwrap());
    assert_eq!(four, [3, 4, 5, 6]);
    assert!(!r.read(&mut two).unwrap());
    assert!(r.read(&mut []).unwrap());
}

#[test]
fn commit_beyond_free_space_or_available_elements_is_refused() {
    let mut w = MessageQueue::<u8>::create(4, false).unwrap();
    let mut r = attach(&w);
    assert_eq!(
        w.commit_write(5).unwrap_err(),
        Error::BadValue("commit exceeds the space that was free")
    );
    assert_eq!(
        r.commit_read(1).unwrap_err(),
        Error::BadValue("commit exceeds the elements available")
    );
    w.commit_write(0).unwrap();
    assert!(w.write(&[1, 2]).unwrap());
    assert_eq!(
        r.commit_read(3).unwrap_err(),
        Error::BadValue("commit exceeds the elements available")
    );
    r.commit_read(2).unwrap();
    assert_eq!(r.available_to_read().unwrap(), 0);
}

// ---- AC-12.3: blocking ---------------------------------------------------

#[test]
fn write_blocking_returns_only_after_the_reader_frees_space() {
    let mut w = MessageQueue::<u8>::create(4, true).unwrap();
    let mut r = attach(&w);
    assert!(w.write(&[1, 2, 3, 4]).unwrap());

    let started = Instant::now();
    let reader = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        let mut out = [0u8; 2];
        r.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, Some(Duration::from_secs(5)))
            .unwrap();
        assert_eq!(out, [1, 2]);
        r
    });
    w.write_blocking(&[5, 6], NOT_FULL, NOT_EMPTY, Some(Duration::from_secs(5)))
        .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    // Well under the 5 s timeout: the reader's wake ended the wait, not the retry after it.
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    let mut r = reader.join().unwrap();
    let mut out = [0u8; 4];
    assert!(r.read(&mut out).unwrap());
    assert_eq!(out, [3, 4, 5, 6]);
}

#[test]
fn write_blocking_times_out_on_a_full_queue() {
    let mut w = MessageQueue::<u8>::create(2, true).unwrap();
    assert!(w.write(&[1, 2]).unwrap());
    let started = Instant::now();
    assert_eq!(
        w.write_blocking(&[3], NOT_FULL, NOT_EMPTY, Some(Duration::from_millis(60)))
            .unwrap_err(),
        Error::TimedOut
    );
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(60), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    // The item was not written.
    assert_eq!(w.available_to_write().unwrap(), 0);
}

#[test]
fn a_timeout_past_the_clock_waits_without_a_deadline() {
    let mut q = MessageQueue::<u8>::create(2, true).unwrap();
    for timeout in [Duration::MAX, Duration::from_secs(i64::MAX as u64)] {
        q.write_blocking(&[1], NOT_FULL, NOT_EMPTY, Some(timeout))
            .unwrap();
        let mut out = [0u8];
        q.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, Some(timeout))
            .unwrap();
        assert_eq!(out, [1]);
    }

    // A write that really sleeps: neither timeout may become a past deadline (`TimedOut`).
    for timeout in [Duration::MAX, Duration::from_secs(i64::MAX as u64)] {
        let mut w = MessageQueue::<u8>::create(2, true).unwrap();
        let mut r = attach(&w);
        w.write(&[1, 2]).unwrap();
        let started = std::time::Instant::now();
        let reader = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            let mut out = [0u8];
            r.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, None)
                .unwrap();
            out
        });
        w.write_blocking(&[3], NOT_FULL, NOT_EMPTY, Some(timeout))
            .unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(40),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(reader.join().unwrap(), [1]);
    }
}

#[test]
fn read_blocking_wakes_on_the_writers_notification() {
    let mut w = MessageQueue::<u32>::create(4, true).unwrap();
    let mut r = attach(&w);
    let started = Instant::now();
    let writer = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        w.write_blocking(&[9, 8, 7], NOT_FULL, NOT_EMPTY, None)
            .unwrap();
    });
    let mut out = [0u32; 3];
    r.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, Some(Duration::from_secs(5)))
        .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_millis(100), "{elapsed:?}");
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(out, [9, 8, 7]);
    writer.join().unwrap();

    let started = Instant::now();
    assert_eq!(
        r.read_blocking(
            &mut out,
            NOT_EMPTY,
            NOT_FULL,
            Some(Duration::from_millis(40))
        )
        .unwrap_err(),
        Error::TimedOut
    );
    assert!(started.elapsed() >= Duration::from_millis(40));
}

#[test]
fn blocking_rejects_a_request_the_queue_can_never_satisfy() {
    let mut q = MessageQueue::<u8>::create(2, true).unwrap();
    assert_eq!(
        q.write_blocking(&[0; 3], NOT_FULL, NOT_EMPTY, None)
            .unwrap_err(),
        Error::BadValue("more items than the queue holds")
    );
    let mut out = [0u8; 3];
    assert_eq!(
        q.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, None)
            .unwrap_err(),
        Error::BadValue("more items than the queue holds")
    );
    assert_eq!(
        q.write_blocking(&[0], 0, NOT_EMPTY, None).unwrap_err(),
        Error::BadValue("empty wait mask")
    );
}

/// The writer wakes on every write, not only on empty to non-empty.
#[test]
fn reader_waiting_for_several_elements_is_woken_by_each_write() {
    let mut w = MessageQueue::<u8>::create(8, true).unwrap();
    let mut r = attach(&w);
    let writer = std::thread::spawn(move || {
        for v in 0..3u8 {
            std::thread::sleep(Duration::from_millis(40));
            w.write_blocking(&[v], NOT_FULL, NOT_EMPTY, None).unwrap();
        }
    });
    let mut out = [0u8; 3];
    let started = Instant::now();
    r.read_blocking(&mut out, NOT_EMPTY, NOT_FULL, Some(Duration::from_secs(5)))
        .unwrap();
    let elapsed = started.elapsed();
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(out, [0, 1, 2]);
    writer.join().unwrap();
}

// ---- AC-12.4: attach checks ---------------------------------------------

fn fresh_desc(capacity: usize, event_flag: bool) -> (MessageQueue<u32>, Descriptor) {
    let q = MessageQueue::<u32>::create(capacity, event_flag).unwrap();
    let d = q.descriptor().unwrap();
    (q, d)
}

#[test]
fn attach_rejects_a_grantor_past_the_end_of_the_fd() {
    let (_q, mut d) = fresh_desc(16, true);
    let size = rsbinder_fmq::shm::region_size(d.fds[0].as_fd()).unwrap();
    d.grantors[2].extent = size; // offset 16 + size > size
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor extends past the end of its fd"
    );
}

#[test]
fn attach_rejects_an_unsealed_memfd_when_the_policy_requires_seals() {
    // A memfd made without seals, laid out like the default layout.
    let fd: OwnedFd = rustix::fs::memfd_create(
        "unsealed",
        rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING,
    )
    .unwrap();
    rustix::fs::ftruncate(&fd, 4096).unwrap();
    let grantors = vec![
        Grantor {
            fd_index: 0,
            offset: 0,
            extent: 8,
        },
        Grantor {
            fd_index: 0,
            offset: 8,
            extent: 8,
        },
        Grantor {
            fd_index: 0,
            offset: 16,
            extent: 64,
        },
        Grantor {
            fd_index: 0,
            offset: 80,
            extent: 4,
        },
    ];
    let d = Descriptor {
        fds: vec![fd],
        ints: Vec::new(),
        grantors,
        quantum: 4,
        flavor: Flavor::SynchronizedReadWrite,
    };
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(16, true))),
        "fd is neither shrink-sealed nor ashmem"
    );
    let lax = AttachPolicy {
        require_seal: false,
        ..policy(16, true)
    };
    let mut q = MessageQueue::<u32>::attach(&d, &lax).unwrap();
    assert_eq!(q.capacity(), 16);
    assert!(q.write(&[1]).unwrap());

    // With `F_SEAL_SHRINK` added later, the strict policy accepts it.
    rustix::fs::fcntl_add_seals(&d.fds[0], rustix::fs::SealFlags::SHRINK).unwrap();
    assert!(rsbinder_fmq::shm::shrink_sealed(d.fds[0].as_fd()));
    MessageQueue::<u32>::attach(&d, &policy(16, true)).unwrap();
}

#[test]
fn attach_rejects_an_unaligned_offset() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[1].offset = 12;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor offset not 8-byte aligned"
    );
}

#[test]
fn attach_rejects_an_fd_index_out_of_range() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[0].fd_index = 1;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor fd index out of range"
    );
}

#[test]
fn attach_rejects_too_few_grantors() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors.truncate(2);
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, false))),
        "fewer than three grantors"
    );
}

#[test]
fn attach_rejects_a_missing_event_flag_only_when_required() {
    let (_q, d) = fresh_desc(16, false);
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "no EventFlag word"
    );
    let q = MessageQueue::<u32>::attach(&d, &policy(1 << 20, false)).unwrap();
    assert!(q.event_flag().is_none());
}

#[test]
fn attach_rejects_a_capacity_above_the_policy() {
    let (_q, d) = fresh_desc(16, true);
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(15, true))),
        "capacity above the attach policy's maximum"
    );
    MessageQueue::<u32>::attach(&d, &policy(16, true)).unwrap();
}

#[test]
fn attach_rejects_a_quantum_that_differs_from_the_element() {
    let (_q, d) = fresh_desc(16, true);
    match MessageQueue::<u16>::attach(&d, &policy(1 << 20, true)) {
        Err(Error::BadValue(what)) => assert_eq!(what, "quantum differs from the element size"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn attach_rejects_a_data_extent_that_is_not_whole_elements() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[2].extent -= 2;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "data extent not a multiple of the quantum"
    );
}

#[test]
fn attach_rejects_overlapping_regions() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[2].offset = 8; // the ring now covers the write counter
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor regions overlap"
    );
}

/// A `dup` of the queue's fd names the same file, so a ring on it may not cover the counters.
#[test]
fn attach_rejects_overlapping_regions_on_a_duplicated_fd() {
    let (_q, mut d) = fresh_desc(16, true);
    d.fds.push(d.fds[0].try_clone().unwrap());
    d.grantors[2].fd_index = 1;
    // Where the default layout puts it, through the dup: no overlap.
    MessageQueue::<u32>::attach(&d, &policy(16, true)).unwrap();
    d.grantors[2].offset = 0;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(16, true))),
        "grantor regions overlap"
    );
}

#[test]
fn attach_rejects_the_unsynchronized_flavor() {
    let (_q, mut d) = fresh_desc(16, true);
    d.flavor = Flavor::UnsynchronizedWrite;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "only the synchronized flavor is supported"
    );
}

#[test]
fn attach_rejects_extents_below_the_minimum_for_the_role() {
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[3].extent = 2;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor extent below the minimum for its role"
    );
    let (_q, mut d) = fresh_desc(16, true);
    d.grantors[0].extent = 4;
    assert_eq!(
        bad(MessageQueue::<u32>::attach(&d, &policy(1 << 20, true))),
        "grantor extent below the minimum for its role"
    );
}

#[test]
fn attach_ignores_grantors_past_the_event_flag_word() {
    let (mut w, mut d) = fresh_desc(16, true);
    d.grantors.push(Grantor {
        fd_index: 9,
        offset: 3,
        extent: 0,
    });
    let mut r = MessageQueue::<u32>::attach(&d, &policy(16, true)).unwrap();
    assert!(w.write(&[42]).unwrap());
    let mut out = [0u32];
    assert!(r.read(&mut out).unwrap());
    assert_eq!(out, [42]);
}

/// libfmq's `bufferFd` constructor: the ring on a second fd, counters and word on the first.
#[test]
fn attach_accepts_a_ring_on_a_second_fd() {
    let flags = rustix::fs::MemfdFlags::CLOEXEC | rustix::fs::MemfdFlags::ALLOW_SEALING;
    let meta = rustix::fs::memfd_create("meta", flags).unwrap();
    rustix::fs::ftruncate(&meta, 4096).unwrap();
    rustix::fs::fcntl_add_seals(
        &meta,
        rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
    )
    .unwrap();
    let ring = rustix::fs::memfd_create("ring", flags).unwrap();
    rustix::fs::ftruncate(&ring, 8192).unwrap();
    rustix::fs::fcntl_add_seals(
        &ring,
        rustix::fs::SealFlags::SHRINK | rustix::fs::SealFlags::GROW,
    )
    .unwrap();
    let d = Descriptor {
        fds: vec![meta, ring],
        ints: vec![],
        grantors: vec![
            Grantor {
                fd_index: 0,
                offset: 0,
                extent: 8,
            },
            Grantor {
                fd_index: 0,
                offset: 8,
                extent: 8,
            },
            Grantor {
                fd_index: 1,
                offset: 0,
                extent: 8192,
            },
            Grantor {
                fd_index: 0,
                offset: 16,
                extent: 4,
            },
        ],
        quantum: 8,
        flavor: Flavor::SynchronizedReadWrite,
    };
    let mut w = MessageQueue::<u64>::attach(&d, &policy(1024, true)).unwrap();
    let mut r = MessageQueue::<u64>::attach(&d, &policy(1024, true)).unwrap();
    assert_eq!(w.capacity(), 1024);
    let items: Vec<u64> = (0..1024).map(|i| i * 7).collect();
    assert!(w.write(&items).unwrap());
    assert!(!w.write(&[1]).unwrap());
    let mut out = vec![0u64; 1024];
    assert!(r.read(&mut out).unwrap());
    assert_eq!(out, items);
}

#[test]
fn corrupted_counters_are_reported_on_every_operation() {
    let (mut w, d) = fresh_desc(16, true);
    let mut r = MessageQueue::<u32>::attach(&d, &policy(16, true)).unwrap();
    let raw = Counters::map(&d);

    raw.set(8, 0);
    assert_eq!(
        w.available_to_write().unwrap_err(),
        Error::Corrupted("read counter ahead of the write counter")
    );
    assert_eq!(
        r.read(&mut [0u32; 1]).unwrap_err(),
        Error::Corrupted("read counter ahead of the write counter")
    );

    raw.set(0, 16 * 4 + 4);
    assert_eq!(
        r.available_to_read().unwrap_err(),
        Error::Corrupted("more bytes in flight than the ring holds")
    );
    assert_eq!(
        w.write(&[1]).unwrap_err(),
        Error::Corrupted("more bytes in flight than the ring holds")
    );
    assert_eq!(
        w.write_blocking(&[1], NOT_FULL, NOT_EMPTY, None)
            .unwrap_err(),
        Error::Corrupted("more bytes in flight than the ring holds")
    );

    raw.set(2, 6);
    assert_eq!(
        r.begin_read(1).unwrap_err(),
        Error::Corrupted("counter not a multiple of the element size")
    );
    assert_eq!(
        w.commit_write(1).unwrap_err(),
        Error::Corrupted("counter not a multiple of the element size")
    );

    // Passes the three checks above, and a commit would wrap it.
    raw.set(u64::MAX - 7, u64::MAX - 7);
    assert_eq!(
        w.write(&[1, 2]).unwrap_err(),
        Error::Corrupted("counter too close to wrapping")
    );
    assert_eq!(
        r.available_to_read().unwrap_err(),
        Error::Corrupted("counter too close to wrapping")
    );

    // Exactly full is not corruption.
    raw.set(0, 16 * 4);
    assert_eq!(w.available_to_write().unwrap(), 0);
    assert_eq!(r.available_to_read().unwrap(), 16);
}

// ---- AC-12.5: pages are allocated at create ------------------------------

#[test]
fn create_allocates_every_page_up_front_and_seals_the_fd() {
    let q = MessageQueue::<u8>::create(3 * 4096 + 100, true).unwrap();
    let d = q.descriptor().unwrap();
    let st = rustix::fs::fstat(&d.fds[0]).unwrap();
    let size = st.st_size as u64;
    assert!(size >= 16 + 3 * 4096 + 100 + 4);
    assert_eq!(size % 4096, 0, "page-rounded like libfmq's ashmem region");
    // `fallocate` backs every page up front, charging the memory to the creating process.
    assert!(
        st.st_blocks as u64 * 512 >= size,
        "st_blocks {} × 512 < size {size}",
        st.st_blocks
    );
    let seals = rustix::fs::fcntl_get_seals(&d.fds[0]).unwrap();
    use rustix::fs::SealFlags;
    assert!(seals.contains(SealFlags::GROW | SealFlags::SHRINK | SealFlags::SEAL));
    assert!(!seals.intersects(SealFlags::WRITE | SealFlags::FUTURE_WRITE));
    // `F_SEAL_SEAL`: a receiver cannot add a seal that blocks the creator's writes.
    assert_eq!(
        rustix::fs::fcntl_add_seals(&d.fds[0], SealFlags::WRITE).unwrap_err(),
        rustix::io::Errno::PERM
    );
}

#[test]
fn create_rejects_impossible_sizes() {
    assert_eq!(
        MessageQueue::<u8>::create(0, true).unwrap_err(),
        Error::BadValue("capacity of zero")
    );
    assert_eq!(
        MessageQueue::<u64>::create(usize::MAX / 2, true).unwrap_err(),
        Error::BadValue("queue too large")
    );
    assert_eq!(
        MessageQueue::<u8>::create(i32::MAX as usize + 1, true).unwrap_err(),
        Error::BadValue("queue too large")
    );
    // Past the `i32::MAX - page_size` bound.
    assert_eq!(
        MessageQueue::<u8>::create(i32::MAX as usize - 16, false).unwrap_err(),
        Error::BadValue("queue too large")
    );
}

// ---- AC-12.6: EventFlag handle -------------------------------------------

#[test]
fn event_flag_outlives_the_queue_and_wakes_across_threads() {
    let q = MessageQueue::<u8>::create(8, true).unwrap();
    let flag = q.event_flag().unwrap();
    let waker = flag.clone();
    drop(q);

    let started = Instant::now();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        waker.wake(NOT_EMPTY).unwrap();
    });
    assert_eq!(
        flag.wait(NOT_EMPTY, Some(Duration::from_secs(5))).unwrap(),
        NOT_EMPTY
    );
    assert!(started.elapsed() >= Duration::from_millis(80));
    t.join().unwrap();

    // The wait consumed the bit.
    assert_eq!(flag.peek(), 0);
    assert_eq!(
        flag.wait(NOT_EMPTY, Some(Duration::from_millis(30)))
            .unwrap_err(),
        Error::TimedOut
    );
}

#[test]
fn wake_merges_into_a_deferred_wake_and_wait_consumes_only_its_mask() {
    let q = MessageQueue::<u8>::create(8, true).unwrap();
    let flag = q.event_flag().unwrap();
    flag.wake(NOT_FULL).unwrap();
    flag.wake(NOT_FULL).unwrap();
    flag.wake(0x4).unwrap();
    assert_eq!(flag.peek(), NOT_FULL | 0x4);
    // Returns at once, clears only NOT_FULL, leaves the other bit.
    assert_eq!(
        flag.wait(NOT_FULL, Some(Duration::from_millis(1))).unwrap(),
        NOT_FULL
    );
    assert_eq!(flag.peek(), 0x4);
    assert_eq!(
        flag.wait(NOT_FULL, Some(Duration::from_millis(20)))
            .unwrap_err(),
        Error::TimedOut
    );
    // A mask covering both returns both.
    flag.wake(NOT_EMPTY).unwrap();
    assert_eq!(flag.wait(NOT_EMPTY | 0x4, None).unwrap(), NOT_EMPTY | 0x4);
    assert_eq!(flag.peek(), 0);
    assert_eq!(
        flag.wait(0, None).unwrap_err(),
        Error::BadValue("empty bit mask")
    );
    flag.wake(0).unwrap();
}

#[test]
fn a_waiter_sees_only_the_bits_of_its_own_mask() {
    let q = MessageQueue::<u8>::create(8, true).unwrap();
    let flag = q.event_flag().unwrap();
    let waker = flag.clone();
    let started = Instant::now();
    let t = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(60));
        waker.wake(NOT_FULL).unwrap(); // not in the waiter's mask: no wake
        std::thread::sleep(Duration::from_millis(60));
        waker.wake(NOT_EMPTY).unwrap();
    });
    assert_eq!(
        flag.wait(NOT_EMPTY, Some(Duration::from_secs(5))).unwrap(),
        NOT_EMPTY
    );
    assert!(started.elapsed() >= Duration::from_millis(100));
    assert_eq!(flag.peek(), NOT_FULL);
    t.join().unwrap();
}

#[test]
fn send_but_not_sync() {
    fn is_send<T: Send>() {}
    fn is_sync<T: Sync>() {}
    is_send::<MessageQueue<u8>>();
    is_send::<rsbinder_fmq::EventFlag>();
    is_sync::<rsbinder_fmq::EventFlag>();
    is_send::<Descriptor>();
    is_sync::<Descriptor>();
    // `!Sync` is checked by the `compile_fail` doctest on `MessageQueue`.
}

// ---- Ring memory is reached only through atomics --------------------------

/// A plain access to ring memory cannot be observed through a real mapping: the source is read.
#[test]
fn ring_code_makes_no_plain_copy_of_ring_memory() {
    // Code only: comment and doc lines may name the very calls this test forbids.
    // Embedded at build time, so the test also runs where the source tree is absent (a device).
    let source = |file: &str| {
        let text = match file {
            "queue.rs" => include_str!("../src/queue.rs"),
            "ring.rs" => include_str!("../src/ring.rs"),
            _ => panic!("{file} is not embedded"),
        };
        text.lines()
            .filter(|line| !line.trim_start().starts_with("//"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    const PLAIN_ACCESSES: [&str; 17] = [
        "copy_nonoverlapping",
        "ptr::copy",
        "copy_from_slice",
        "clone_from_slice",
        "copy_within",
        ".copy_to",
        "read_volatile",
        "write_volatile",
        "read_unaligned",
        "write_unaligned",
        "ptr::read",
        "ptr::write",
        ".read(",
        ".write(",
        "as_ref(",
        "as_mut(",
        "from_ptr_range",
    ];
    // The queue's own `read`/`write` calls, and the counter's `&AtomicU64`.
    const ALLOWED: [&str; 3] = [
        "if self.write(items)? {",
        "if self.read(out)? {",
        "unsafe { self.ptr.as_ref() }",
    ];
    for file in ["queue.rs", "ring.rs"] {
        let text = source(file);
        for line in text.lines().filter(|l| !ALLOWED.contains(&l.trim())) {
            for pattern in PLAIN_ACCESSES {
                assert!(!line.contains(pattern), "{file} uses `{pattern}`: {line}");
            }
        }
    }
    // `queue.rs` builds no slice at all; `ring.rs` builds atomics or views of a caller's slice.
    assert!(!source("queue.rs").contains("from_raw_parts"));
    let ring = source("ring.rs");
    let calls: Vec<&str> = ring
        .match_indices("from_raw_parts")
        .map(|(at, _)| {
            let args = &ring[at..];
            let mut depth = 0;
            let end = args
                .char_indices()
                .find_map(|(i, c)| {
                    match c {
                        '(' => depth += 1,
                        ')' if depth == 1 => return Some(i),
                        ')' => depth -= 1,
                        _ => {}
                    }
                    None
                })
                .expect("a closed call");
            &args[..end]
        })
        .collect();
    assert_eq!(calls.len(), 5, "{calls:#?}");
    for call in calls {
        assert!(call.contains("Atomic") || call.contains("(user."), "{call}");
    }
}

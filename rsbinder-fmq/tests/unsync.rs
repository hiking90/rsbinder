// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The unsynchronized flavor in one process: a writer and readers attached through its
//! descriptor, each with mappings of its own. Plan 15 AC-15.1.4 (the mapping half), 15.1.14.
//! Two processes are `tests/unsync_two_process.rs`; libfmq is `tests/libfmq_host.rs`.
//!
//! The file is compiled for targets on which `UnsyncReader` exists: `attach` needs
//! `shm::RoWord64` and is `Unsupported` without it. The `cfg` below names no more than those
//! targets (every 64-bit one that runs Linux or Android, 32-bit x86 with SSE2, 32-bit ARM with
//! 64-bit atomics); what `attach` returns on the others is a unit test in `src/lib.rs`.

#![cfg(all(
    any(target_os = "linux", target_os = "android"),
    target_has_atomic = "64",
    any(
        target_pointer_width = "64",
        all(target_arch = "x86", target_feature = "sse2"),
        target_arch = "arm"
    )
))]

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rsbinder_fmq::shm::{self, Access, CreateOptions, Mapping, Run, Seals};
use rsbinder_fmq::{
    AttachPolicy, Descriptor, Errno, Error, Flavor, Grantor, MessageQueue, UnsyncCreate,
    UnsyncRead, UnsyncReader, UnsyncStart, UnsyncWriter,
};

const POLICY: AttachPolicy = AttachPolicy::new(1 << 16);

fn open() -> UnsyncCreate {
    UnsyncCreate::new()
}

fn sealed() -> UnsyncCreate {
    UnsyncCreate::new().read_only_readers(true)
}

fn reader<T: rsbinder_fmq::Element>(writer: &UnsyncWriter<T>) -> UnsyncReader<T> {
    let desc = writer.descriptor().expect("descriptor");
    UnsyncReader::attach(&desc, &POLICY, UnsyncStart::Oldest).expect("attach")
}

/// The first 16 bytes of a queue's fd through a writable mapping: what another process could do.
struct Counters(Mapping);

impl Counters {
    fn of(desc: &Descriptor) -> Self {
        Self(Mapping::map(desc.fds[0].as_fd(), 0, 16, Access::ReadWrite).expect("map"))
    }

    fn word(&self, index: usize) -> &AtomicU64 {
        // SAFETY: an 8-aligned word of a live page-aligned mapping, reached only through atomics.
        unsafe { self.0.as_ptr().add(8 * index).cast::<AtomicU64>().as_ref() }
    }

    fn end(&self) -> &AtomicU64 {
        self.word(Descriptor::WRITE_REGION_END)
    }

    fn write(&self) -> &AtomicU64 {
        self.word(Descriptor::WRITE_COUNTER)
    }
}

// ---- What `create` makes ---------------------------------------------------

#[test]
fn a_new_queue_is_three_grantors_of_the_unsynchronized_flavor() {
    let writer = UnsyncWriter::<u32>::create(64, &open()).unwrap();
    assert_eq!(
        (writer.capacity(), writer.quantum(), writer.written()),
        (64, 4, 0)
    );
    let desc = writer.descriptor().unwrap();
    assert_eq!(desc.flavor, Flavor::UnsynchronizedWrite);
    assert_eq!(desc.quantum, 4);
    assert!(!desc.has_event_flag());
    let grantor = |offset, extent| Grantor {
        fd_index: 0,
        offset,
        extent,
    };
    assert_eq!(
        desc.grantors,
        vec![grantor(0, 8), grantor(8, 8), grantor(16, 256)]
    );
    let page = rustix::param::page_size() as u64;
    assert_eq!(shm::region_size(desc.fds[0].as_fd()).unwrap(), page);
    assert_eq!(
        shm::seals(desc.fds[0].as_fd()).unwrap(),
        Seals::GROW | Seals::SHRINK | Seals::SEAL
    );
}

#[test]
fn read_only_readers_seals_the_fd_against_writable_mappings() {
    let mut writer = UnsyncWriter::<u32>::create(64, &sealed()).unwrap();
    let desc = writer.descriptor().unwrap();
    assert_eq!(
        shm::seals(desc.fds[0].as_fd()).unwrap(),
        Seals::GROW | Seals::SHRINK | Seals::SEAL | Seals::FUTURE_WRITE
    );
    let fd = desc.fds[0].as_fd();
    assert_eq!(
        Mapping::map(fd, 0, 16, Access::ReadWrite).err(),
        Some(Error::Os(Errno::from_raw(
            rustix::io::Errno::PERM.raw_os_error()
        )))
    );
    // The writer's mapping predates the seal, and a reader needs no more than `PROT_READ`.
    let mut reader = reader(&writer);
    writer.write(&[7, 8, 9]).unwrap();
    let mut out = [0u32; 3];
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [7, 8, 9]);
}

#[test]
fn create_refuses_a_ring_no_write_fits() {
    let bad = |capacity| match UnsyncWriter::<u32>::create(capacity, &open()) {
        Err(Error::BadValue(what)) => what,
        other => panic!("expected BadValue, got {other:?}"),
    };
    assert_eq!(bad(0), "capacity of zero");
    assert_eq!(bad(1), "capacity below two");
    assert_eq!(bad(usize::MAX / 2), "queue too large");
    assert!(UnsyncWriter::<u32>::create(2, &open()).is_ok());
}

// ---- Reading in order ------------------------------------------------------

#[test]
fn every_reader_gets_every_element_in_order() {
    let mut writer = UnsyncWriter::<u32>::create(16, &sealed()).unwrap();
    let mut readers = [reader(&writer), reader(&writer), reader(&writer)];
    let mut next = [0u32; 3];
    // 600 elements through a ring of 16, never more than 15 ahead of the slowest reader.
    for round in 0..120u32 {
        let items: Vec<u32> = (round * 5..round * 5 + 5).collect();
        writer.write(&items).unwrap();
        for (index, reader) in readers.iter_mut().enumerate() {
            // Reader 0 reads each round, reader 1 every second, reader 2 every third.
            if (round + 1) % (index as u32 + 1) != 0 {
                continue;
            }
            let mut out = vec![0u32; 5 * (index + 1)];
            assert_eq!(reader.available_to_read().unwrap(), out.len());
            assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
            for value in out {
                assert_eq!(value, next[index]);
                next[index] += 1;
            }
            assert_eq!(reader.read(&mut [0u32; 1]).unwrap(), UnsyncRead::Empty);
        }
    }
    assert_eq!(next, [600, 600, 600]);
    assert_eq!(writer.written(), 600);
}

#[test]
fn bytes_cross_the_wrap_at_every_alignment() {
    // 13 bytes: every run starts and ends off a word boundary sooner or later.
    let mut writer = UnsyncWriter::<u8>::create(13, &sealed()).unwrap();
    let mut reader = reader(&writer);
    let mut sent = 0u32;
    for round in 0..200usize {
        let n = round % 12 + 1;
        let items: Vec<u8> = (0..n).map(|i| (sent + i as u32) as u8).collect();
        writer.write(&items).unwrap();
        let mut out = vec![0u8; n];
        assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
        assert_eq!(out, items, "round {round}");
        sent += n as u32;
    }
}

#[test]
fn an_empty_queue_and_a_short_one_read_nothing() {
    let mut writer = UnsyncWriter::<u64>::create(8, &open()).unwrap();
    let mut reader = reader(&writer);
    let mut out = [5u64; 2];
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Empty);
    writer.write(&[1]).unwrap();
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Empty);
    assert_eq!(out, [5, 5]);
    assert_eq!(reader.read(&mut []).unwrap(), UnsyncRead::Done);
    writer.write(&[2]).unwrap();
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [1, 2]);
}

// ---- Overwriting -----------------------------------------------------------

#[test]
fn a_lapped_reader_loses_all_but_half_a_ring() {
    let mut writer = UnsyncWriter::<u32>::create(16, &sealed()).unwrap();
    let mut slow = reader(&writer);
    for value in 0..100u32 {
        writer.write(&[value]).unwrap();
    }
    assert_eq!(slow.available_to_read().unwrap(), 100);
    let mut out = [0u32; 8];
    // The move is from element 0 to element 92, half a ring behind the 100 written.
    assert_eq!(
        slow.read(&mut out).unwrap(),
        UnsyncRead::Lost { elements: 92 }
    );
    assert_eq!(slow.available_to_read().unwrap(), 8);
    assert_eq!(slow.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [92, 93, 94, 95, 96, 97, 98, 99]);
}

/// Plan 15 §4.2: a reservation filled whole and committed in part exposes nothing it overwrote.
#[test]
fn a_partial_commit_keeps_the_region_end() {
    let mut writer = UnsyncWriter::<u32>::create(16, &open()).unwrap();
    let desc = writer.descriptor().unwrap();
    let counters = Counters::of(&desc);
    let mut reader = reader(&writer);
    let filler: Vec<u32> = (0..8).collect();
    writer.write(&filler).unwrap();
    writer.write(&filler).unwrap();
    assert_eq!(reader.read(&mut [0u32; 4]).unwrap(), UnsyncRead::Done);

    // write = 16, read = 4. Eight elements are written over elements 0 to 7; two are committed.
    let mut txn = writer.begin_write(8).unwrap();
    txn.write_at(0, &[100; 8]).unwrap();
    assert_eq!(counters.end().load(Ordering::Acquire), 24 * 4);
    txn.commit_first(2).unwrap();
    // A shorter reservation from the new write counter does not lower the end.
    drop(writer.begin_write(1).unwrap());
    assert_eq!(counters.write().load(Ordering::Acquire), 18 * 4);
    assert_eq!(counters.end().load(Ordering::Acquire), 24 * 4);
    assert_eq!(writer.written(), 18);

    // The reader stood at element 4, which is gone: with a lowered end `18 - 4 <= 16` would pass.
    let mut out = [0u32; 4];
    assert_eq!(
        reader.read(&mut out).unwrap(),
        UnsyncRead::Lost { elements: 6 }
    );
    let mut rest = [0u32; 8];
    assert_eq!(reader.read(&mut rest).unwrap(), UnsyncRead::Done);
    assert_eq!(rest, [2, 3, 4, 5, 6, 7, 100, 100]);

    // A commit is at most its transaction, and the end moves on with the next reservation.
    let txn = writer.begin_write(6).unwrap();
    assert_eq!(counters.end().load(Ordering::Acquire), 24 * 4);
    assert_eq!(
        txn.commit_first(7),
        Err(Error::BadValue("commit exceeds the elements reserved"))
    );
    assert_eq!(writer.written(), 18, "a refused commit publishes nothing");
    writer.begin_write(6).unwrap().commit().unwrap();
    drop(writer.begin_write(1).unwrap());
    assert_eq!(counters.end().load(Ordering::Acquire), 25 * 4);
}

/// A dropped transaction publishes nothing; its announcement stays, as a partial commit's does.
#[test]
fn a_dropped_write_transaction_publishes_nothing() {
    let mut writer = UnsyncWriter::<u32>::create(8, &open()).unwrap();
    let desc = writer.descriptor().unwrap();
    let counters = Counters::of(&desc);
    let mut reader = reader(&writer);
    {
        let mut txn = writer.begin_write(3).unwrap();
        txn.write_at(0, &[9, 9, 9]).unwrap();
    }
    assert_eq!(writer.written(), 0);
    assert_eq!(counters.write().load(Ordering::Acquire), 0);
    assert_eq!(counters.end().load(Ordering::Acquire), 3 * 4);
    writer.write(&[1, 2]).unwrap();
    let mut out = [0u32; 2];
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [1, 2]);
}

#[test]
fn a_write_is_at_most_the_capacity_less_one() {
    let mut writer = UnsyncWriter::<u32>::create(8, &open()).unwrap();
    let refusal = Error::BadValue("more elements than the capacity less one");
    assert_eq!(writer.write(&[0; 8]), Err(refusal));
    assert_eq!(writer.write(&[0; 9]), Err(refusal));
    assert_eq!(writer.begin_write(8).err(), Some(refusal));
    assert_eq!(writer.begin_write(usize::MAX).err(), Some(refusal));
    assert_eq!(writer.written(), 0);
    writer.write(&[0; 7]).unwrap();
    assert_eq!(writer.written(), 7);
}

#[test]
fn a_read_longer_than_the_capacity_is_refused() {
    let writer = UnsyncWriter::<u32>::create(8, &open()).unwrap();
    let mut reader = reader(&writer);
    assert_eq!(
        reader.read(&mut [0u32; 9]),
        Err(Error::BadValue("more elements than the ring holds"))
    );
    assert_eq!(reader.read(&mut [0u32; 8]).unwrap(), UnsyncRead::Empty);
}

// ---- Where a reader starts -------------------------------------------------

#[test]
fn a_reader_started_now_gets_what_is_written_after_it_attached() {
    let mut writer = UnsyncWriter::<u32>::create(8, &sealed()).unwrap();
    for value in 0..21u32 {
        writer.write(&[value]).unwrap();
    }
    let desc = writer.descriptor().unwrap();
    let mut now = UnsyncReader::<u32>::attach(&desc, &POLICY, UnsyncStart::Now).unwrap();
    let mut oldest = UnsyncReader::<u32>::attach(&desc, &POLICY, UnsyncStart::Oldest).unwrap();
    assert_eq!(now.available_to_read().unwrap(), 0);
    assert_eq!(oldest.available_to_read().unwrap(), 21);
    let mut out = [0u32; 2];
    assert_eq!(now.read(&mut out).unwrap(), UnsyncRead::Empty);
    writer.write(&[21, 22]).unwrap();
    assert_eq!(now.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [21, 22]);
    // The other one starts at zero, as libfmq does, and is told what the ring no longer holds.
    assert_eq!(
        oldest.read(&mut out).unwrap(),
        UnsyncRead::Lost { elements: 19 }
    );
    assert_eq!(oldest.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [19, 20]);
}

// ---- The latest elements ---------------------------------------------------

#[test]
fn read_latest_takes_the_newest_and_consumes_everything() {
    let mut writer = UnsyncWriter::<u32>::create(8, &sealed()).unwrap();
    let mut reader = reader(&writer);
    let mut out = [0u32; 3];
    assert_eq!(reader.read_latest(&mut out).unwrap().copied, 0);
    writer.write(&[1, 2]).unwrap();
    assert_eq!(reader.read_latest(&mut out).unwrap().copied, 2);
    assert_eq!(out[..2], [1, 2]);
    assert_eq!(reader.read_latest(&mut out).unwrap().copied, 0);

    // 27 more through a ring of 8: lapped, and the newest three are still there.
    for value in 3..30u32 {
        writer.write(&[value]).unwrap();
    }
    assert_eq!(reader.available_to_read().unwrap(), 27);
    assert_eq!(reader.read_latest(&mut out).unwrap().copied, 3);
    assert_eq!(out, [27, 28, 29]);
    assert_eq!(reader.available_to_read().unwrap(), 0);

    // A buffer of one is the current value; a buffer longer than the ring gets one ring.
    writer.write(&[30, 31, 32]).unwrap();
    assert_eq!(reader.read_latest(&mut out[..1]).unwrap().copied, 1);
    assert_eq!(out[0], 32);
    for value in 33..60u32 {
        writer.write(&[value]).unwrap();
    }
    let mut wide = [0u32; 20];
    assert_eq!(reader.read_latest(&mut wide).unwrap().copied, 8);
    assert_eq!(wide[..8], [52, 53, 54, 55, 56, 57, 58, 59]);
    assert_eq!(reader.read_latest(&mut []).unwrap().copied, 0);
}

#[test]
fn read_latest_leaves_out_what_a_write_in_progress_covers() {
    let mut writer = UnsyncWriter::<u32>::create(8, &sealed()).unwrap();
    let mut reader = reader(&writer);
    for value in 0..8u32 {
        writer.write(&[value]).unwrap();
    }
    // Three elements are reserved over elements 0 to 2; libfmq's `readLatest` returns 0 here.
    drop(writer.begin_write(3).unwrap());
    let mut out = [0u32; 8];
    assert_eq!(reader.read_latest(&mut out).unwrap().copied, 5);
    assert_eq!(out[..5], [3, 4, 5, 6, 7]);
}

// ---- What attach refuses ---------------------------------------------------

#[test]
fn each_flavor_refuses_the_others_descriptor() {
    let unsync = UnsyncWriter::<u32>::create(8, &open()).unwrap();
    let sync = MessageQueue::<u32>::create(8, false).unwrap();
    assert_eq!(
        MessageQueue::<u32>::attach(&unsync.descriptor().unwrap(), &POLICY).err(),
        Some(Error::InvalidDescriptor(
            "descriptor is not of the synchronized flavor"
        ))
    );
    let start = UnsyncStart::Oldest;
    assert_eq!(
        UnsyncReader::<u32>::attach(&sync.descriptor().unwrap(), &POLICY, start).err(),
        Some(Error::InvalidDescriptor(
            "descriptor is not of the unsynchronized flavor"
        ))
    );
}

#[test]
fn attach_applies_the_policy_and_the_quantum() {
    let writer = UnsyncWriter::<u32>::create(64, &open()).unwrap();
    let desc = writer.descriptor().unwrap();
    let start = UnsyncStart::Oldest;
    let bad = |result: Result<_, Error>| match result {
        Err(Error::InvalidDescriptor(what)) => what,
        Err(e) => panic!("expected InvalidDescriptor, got {e:?}"),
        Ok(()) => panic!("expected InvalidDescriptor, attach succeeded"),
    };
    let small = AttachPolicy::new(63);
    assert_eq!(
        bad(UnsyncReader::<u32>::attach(&desc, &small, start).map(drop)),
        "capacity above the attach policy's maximum"
    );
    // A reader of this flavor never maps the word, so the policy does not ask for it.
    let flagged = POLICY.require_event_flag(true);
    assert!(UnsyncReader::<u32>::attach(&desc, &flagged, start).is_ok());
    assert_eq!(
        bad(UnsyncReader::<u64>::attach(&desc, &POLICY, start).map(drop)),
        "quantum differs from the element size"
    );
    assert_eq!(
        UnsyncReader::<u32>::attach(&desc, &POLICY, start)
            .unwrap()
            .capacity(),
        64
    );
}

// ---- A queue this crate did not make ----------------------------------------

/// A four-grantor unsynchronized queue as libfmq lays one out, written through raw mappings.
struct Foreign {
    desc: Descriptor,
    counters: Counters,
    ring: Mapping,
}

impl Foreign {
    const BYTES: u64 = 32;

    fn new() -> Self {
        let page = rustix::param::page_size() as u64;
        let fd = shm::create(page, &CreateOptions::new("foreign").allocate(true)).unwrap();
        shm::add_seals(fd.as_fd(), Seals::GROW | Seals::SHRINK).unwrap();
        let grantor = |offset, extent| Grantor {
            fd_index: 0,
            offset,
            extent,
        };
        let desc = Descriptor {
            fds: vec![fd],
            ints: Vec::new(),
            grantors: vec![
                grantor(0, 8),
                grantor(8, 8),
                grantor(16, Self::BYTES),
                grantor(48, 4),
            ],
            quantum: 4,
            flavor: Flavor::UnsynchronizedWrite,
        };
        let ring = Mapping::map(desc.fds[0].as_fd(), 16, Self::BYTES, Access::ReadWrite).unwrap();
        Self {
            counters: Counters::of(&desc),
            desc,
            ring,
        }
    }

    /// Store `values` at element `position` and both counters as given, the way a writer would.
    fn put(&self, position: u64, values: &[u32], end: u64, write: u64) {
        self.counters.end().store(end, Ordering::Release);
        for (index, value) in values.iter().enumerate() {
            let offset = ((position + index as u64) * 4 % Self::BYTES) as usize;
            // SAFETY: four bytes inside the ring's mapping, written by this thread alone.
            let run = unsafe { Run::new(self.ring.as_ptr().add(offset), 4) };
            run.store_release(&value.to_ne_bytes());
        }
        self.counters.write().store(write, Ordering::Release);
    }

    fn reader(&self) -> UnsyncReader<u32> {
        let policy = POLICY.require_event_flag(true);
        UnsyncReader::attach(&self.desc, &policy, UnsyncStart::Oldest).expect("attach")
    }
}

#[test]
fn a_queue_with_an_event_flag_word_attaches() {
    let queue = Foreign::new();
    let mut reader = queue.reader();
    assert_eq!(reader.capacity(), 8);
    queue.put(0, &[10, 11, 12], 12, 12);
    let mut out = [0u32; 3];
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [10, 11, 12]);
}

/// A libfmq writer before `android-16.0.0_r4` leaves the region end at zero.
#[test]
fn a_writer_that_stores_no_region_end_is_judged_by_the_write_counter() {
    let queue = Foreign::new();
    let mut reader = queue.reader();
    let values: Vec<u32> = (0..8).collect();
    queue.put(0, &values, 0, 32);
    let mut out = [0u32; 4];
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [0, 1, 2, 3]);
    // Six more: 14 written, the reader at 4 with a ring of 8 behind the writer.
    queue.put(8, &[8, 9, 10, 11, 12, 13], 0, 56);
    assert_eq!(
        reader.read(&mut out).unwrap(),
        UnsyncRead::Lost { elements: 6 }
    );
    assert_eq!(reader.read(&mut out).unwrap(), UnsyncRead::Done);
    assert_eq!(out, [10, 11, 12, 13]);
}

#[test]
fn counters_no_writer_produces_end_the_reader() {
    let corrupted = |end: u64, write: u64| {
        let queue = Foreign::new();
        let mut reader = queue.reader();
        queue.put(0, &[1, 2], 8, 8);
        assert_eq!(reader.read(&mut [0u32; 1]).unwrap(), UnsyncRead::Done);
        queue.counters.end().store(end, Ordering::Release);
        queue.counters.write().store(write, Ordering::Release);
        let what = match reader.read(&mut [0u32; 1]) {
            Err(Error::Corrupted(what)) => what,
            other => panic!("expected Corrupted, got {other:?}"),
        };
        // Putting the counters right does not bring the reader back.
        queue.counters.end().store(8, Ordering::Release);
        queue.counters.write().store(8, Ordering::Release);
        assert_eq!(reader.read(&mut [0u32; 1]), Err(Error::Corrupted(what)));
        assert_eq!(reader.available_to_read(), Err(Error::Corrupted(what)));
        assert_eq!(
            reader.read_latest(&mut [0u32; 1]),
            Err(Error::Corrupted(what))
        );
        what
    };
    // What a libfmq reader's default `resetPointers = true` does to a queue in use.
    assert_eq!(corrupted(0, 0), "write counter moved backwards");
    assert_eq!(corrupted(8, 4), "write counter moved backwards");
    assert_eq!(
        corrupted(10, 10),
        "write counter not a multiple of the element size"
    );
    assert_eq!(
        corrupted(8 + 36, 8),
        "more bytes reserved than the ring holds"
    );
    assert_eq!(corrupted(0, u64::MAX - 31), "counter too close to wrapping");
}

#[test]
fn a_reader_started_now_checks_the_counters_it_starts_from() {
    let queue = Foreign::new();
    queue.put(0, &[], 0, 6);
    assert_eq!(
        UnsyncReader::<u32>::attach(&queue.desc, &POLICY, UnsyncStart::Now).err(),
        Some(Error::Corrupted(
            "write counter not a multiple of the element size"
        ))
    );
    assert!(UnsyncReader::<u32>::attach(&queue.desc, &POLICY, UnsyncStart::Oldest).is_ok());
}

// ---- Threads ----------------------------------------------------------------

/// Elements carry their index, so an overwritten one read as `Done` shows as a break in the run.
#[test]
fn readers_racing_the_writer_get_whole_elements_in_order() {
    let mut writer = UnsyncWriter::<u64>::create(64, &sealed()).unwrap();
    let desc = writer.descriptor().unwrap();
    let stop = AtomicBool::new(false);
    let run_for = Duration::from_millis(300);
    /// Stops the readers when dropped, so that a panic of the writer ends them too.
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    std::thread::scope(|s| {
        // No batch above half the ring: that is all a lapped reader keeps.
        let readers: Vec<_> = [1usize, 7, 32]
            .into_iter()
            .map(|batch| {
                let (desc, stop) = (&desc, &stop);
                s.spawn(move || {
                    let mut reader =
                        UnsyncReader::<u64>::attach(desc, &POLICY, UnsyncStart::Oldest).unwrap();
                    let (mut expected, mut done, mut lost) = (0u64, 0u64, 0u64);
                    let mut out = vec![0u64; batch];
                    while !stop.load(Ordering::Relaxed) {
                        match reader.read(&mut out).unwrap() {
                            UnsyncRead::Done => {
                                for value in &out {
                                    assert_eq!(*value, expected, "batch {batch}");
                                    expected += 1;
                                }
                                done += 1;
                            }
                            UnsyncRead::Lost { elements } => {
                                assert!(elements > 0);
                                expected += elements;
                                lost += 1;
                            }
                            UnsyncRead::Empty => std::hint::spin_loop(),
                        }
                    }
                    (done, lost)
                })
            })
            .collect();
        let stopper = Stop(&stop);
        let started = Instant::now();
        let mut next = 0u64;
        while started.elapsed() < run_for {
            for _ in 0..1000 {
                let n = (next % 13 + 1) as usize;
                let items: Vec<u64> = (next..next + n as u64).collect();
                writer.write(&items).unwrap();
                next += n as u64;
            }
        }
        drop(stopper);
        for reader in readers {
            let (done, _lost) = reader.join().unwrap();
            assert!(done > 0, "a reader never read anything");
        }
    });
}

// ---- Auto traits ------------------------------------------------------------

#[test]
fn both_ends_are_send() {
    fn is_send<T: Send>() {}
    is_send::<UnsyncWriter<u8>>();
    is_send::<UnsyncReader<u8>>();
    // `!Sync` is checked by the `compile_fail` doctests on the two types.
}

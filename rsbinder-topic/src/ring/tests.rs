// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The writer and the reader over heap memory, which is what Miri can run (it has no memfd and
//! no `mmap`). The mapped memory is exercised by `mapped_tests.rs`.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use super::*;
use crate::image::Got;
use crate::layout::Heap;

/// A ring of 128 records of [`SLOT`] bytes.
const RING: usize = 4096;
/// A record with [`BODY`] bytes of payload.
const SLOT: u64 = 32;
const BODY: usize = 24;

/// The record's index over and over, inverted: taken for a header it has a length no record has.
fn body(index: u32, len: usize) -> Vec<u8> {
    (!index)
        .to_le_bytes()
        .into_iter()
        .cycle()
        .take(len)
        .collect()
}

fn item(index: u32) -> Got {
    Got::Item(index, body(index, BODY))
}

fn writer_of(heap: &Heap) -> Writer<impl Stores> {
    Writer::new(heap.stored())
}

fn reader_of(heap: &Heap, start: Start) -> Reader<impl Loads> {
    Reader::attach(heap.loaded(), start).unwrap()
}

/// Publish `count` records of [`BODY`] bytes.
fn publish<M: Stores>(writer: &mut Writer<M>, count: u32) {
    for _ in 0..count {
        let index = writer.next_index();
        writer.put(Kind::Item, &body(index, BODY)).unwrap();
    }
}

fn take<M: Loads>(reader: &mut Reader<M>) -> Got {
    Got::of(reader.try_recv())
}

fn latest<M: Loads>(reader: &mut Reader<M>) -> Got {
    Got::of(reader.try_latest())
}

/// Everything up to the first `Empty`, or up to and with the end record.
fn drain<M: Loads>(reader: &mut Reader<M>) -> Vec<Got> {
    let mut all = Vec::new();
    loop {
        match take(reader) {
            Got::Empty => return all,
            end @ Got::End(_) => {
                all.push(end);
                return all;
            }
            other => all.push(other),
        }
    }
}

#[test]
fn records_come_out_in_the_order_they_went_in() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    assert_eq!(take(&mut reader), Got::Empty);
    assert_eq!(writer.max_payload(), 2040);
    // Every padding from none to seven bytes, and the longest record, across many wraps.
    let lengths = [0, 1, 7, 8, 9, 100, 2040];
    for index in 0..700u32 {
        let payload = body(index, lengths[index as usize % lengths.len()]);
        writer.put(Kind::Item, &payload).unwrap();
        assert_eq!(take(&mut reader), Got::Item(index, payload));
    }
    assert_eq!(take(&mut reader), Got::Empty);
    assert_eq!(reader.lost(), 0);
    assert_eq!(reader.position(), writer.position());
    assert_eq!(writer.next_index(), 700);
}

#[test]
fn a_record_longer_than_half_the_ring_is_refused() {
    // 4104 / 2 is not a multiple of 8; a record is, so the bound is the multiple below it.
    for capacity in [RING, RING + 8] {
        let heap = Heap::new(capacity);
        let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
        assert_eq!(
            writer.put(Kind::Item, &[0; 2041]),
            Err(Refused::TooLarge {
                bytes: 2041,
                max: 2040
            })
        );
        assert_eq!(writer.position(), 0);
        assert_eq!(take(&mut reader), Got::Empty);
        writer.put(Kind::Item, &body(0, 2040)).unwrap();
        assert_eq!(take(&mut reader), Got::Item(0, body(0, 2040)));
    }
}

#[test]
fn a_batch_is_admitted_only_if_put_takes_every_record_of_it() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    assert_eq!(writer.admits([0, 2040, 7]), Ok(()));
    let too_large = Refused::TooLarge {
        bytes: 2041,
        max: 2040,
    };
    assert_eq!(writer.admits([1, 2041, 1]), Err(too_large));
    // Two records of 32 bytes end at the last position below 2^64; one more of 8 does not fit.
    let last = u64::MAX / 8 * 8;
    writer.written = last - 64;
    assert_eq!(writer.admits([24, 24]), Ok(()));
    assert_eq!(writer.admits([24, 24, 0]), Err(Refused::Spent));
    writer.written = 0;
    writer.put(Kind::End, &[]).unwrap();
    assert_eq!(writer.admits([]), Err(Refused::Ended));

    // A batch whose last record would pass 2^64 writes none of the ones before it.
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    writer.written = last - 64;
    let topic = crate::publish::TypedWriter::new(writer);
    let batch = [crate::Bytes(vec![1; 24]), crate::Bytes(vec![2; 24])];
    assert_eq!(
        topic.publish_all(batch.iter().chain([&crate::Bytes(vec![])])),
        Err(crate::PublishError::Closed)
    );
    assert_eq!(heap.loaded().write(), 0);
    assert_eq!(topic.publish_all(&batch), Ok(2));
    assert_eq!(heap.loaded().write(), last);
}

#[test]
fn a_ring_sized_for_a_payload_keeps_that_many_of_the_largest_records_while_it_writes() {
    for (max, depth) in [(2000, 1), (1000, 3), (300, 16), (8, 2), (0, 0)] {
        let ring = crate::PublisherPolicy::for_payload(max, depth).ring_bytes;
        let heap = Heap::new(ring);
        let mut writer = writer_of(&heap);
        let record = 8 + max.div_ceil(8) * 8;
        for _ in 0..ring / record + 2 {
            let index = writer.next_index();
            writer.put(Kind::Item, &body(index, max)).unwrap();
        }
        // The next record's `tail` is stored and none of its bytes yet: the fewest kept.
        writer.stop = Stop::AfterRegionEnd;
        writer.put(Kind::Item, &body(0, max)).unwrap();
        let kept = drain(&mut reader_of(&heap, Start::Oldest)).len();
        if ring > crate::layout::MIN_RING {
            assert_eq!(kept, depth, "{max} bytes, depth {depth}");
        } else {
            assert!(kept >= depth.max(1), "{max} bytes, depth {depth}: {kept}");
        }
    }
}

#[test]
fn the_counters_keep_their_order_after_every_publish() {
    let heap = Heap::new(RING);
    let (mut writer, view) = (writer_of(&heap), heap.loaded());
    let capacity = RING as u64;
    let mut starts = Vec::new();
    let mut state = 12345u32;
    for _ in 0..3000 {
        state = state.wrapping_mul(1103515245).wrapping_add(12345);
        let len = (state >> 16) as usize % 2041;
        starts.push(writer.position());
        writer.put(Kind::Item, &vec![0xa5; len]).unwrap();
        let (tail, half, last, write) = (view.tail(), view.half(), view.last(), view.write());
        assert_eq!(write, writer.position());
        assert_eq!(last, *starts.last().unwrap());
        // The oldest record the ring still holds whole, and the oldest in its newer half.
        let kept = |room: u64| *starts.iter().find(|at| write - **at <= room).unwrap();
        assert_eq!(tail, kept(capacity));
        assert_eq!(half, kept(capacity / 2));
        assert!(tail <= half && half <= last && last < write);
    }
}

#[test]
fn a_lapped_reader_is_told_what_it_lost_and_resumes_half_a_ring_back() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(0));
    publish(&mut writer, 1000);
    // 64 records fit the newer half; the reader resumes at the oldest of them.
    assert_eq!(take(&mut reader), Got::Lagged(936));
    assert_eq!(reader.lost(), 936);
    let rest = drain(&mut reader);
    assert_eq!(rest, (937..1001).map(item).collect::<Vec<_>>());
    assert_eq!(reader.lost(), 936);
}

#[test]
fn a_reader_reported_as_lagged_stays_on_the_record_it_resumed_at() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(0));
    publish(&mut writer, 200);
    assert_eq!(take(&mut reader), Got::Lagged(136));
    // Overwritten before the reader came back for it: the second loss counts from that record.
    publish(&mut writer, 200);
    assert_eq!(take(&mut reader), Got::Lagged(200));
    assert_eq!(take(&mut reader), item(337));
    assert_eq!(reader.lost(), 336);
}

/// A reader at half the publisher's pace: how many records it returns between two losses.
fn runs_between_losses() -> Vec<u32> {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    let (mut runs, mut run, mut next) = (Vec::new(), 0, 0u32);
    for _ in 0..2000 {
        publish(&mut writer, 2);
        match take(&mut reader) {
            Got::Item(index, payload) => {
                assert_eq!((index, &payload), (next, &body(index, BODY)));
                (next, run) = (next + 1, run + 1);
            }
            Got::Lagged(records) => {
                runs.push(run);
                (next, run) = (next + records as u32, 0);
            }
            other => panic!("{other:?}"),
        }
    }
    runs
}

#[test]
fn a_reader_at_half_the_publishers_pace_reads_a_run_after_each_loss() {
    let runs = runs_between_losses();
    assert!(runs.len() > 4, "{runs:?}");
    // The ring holds 128 records. Resuming at `half`, a run is about 64; at `tail` it is none.
    let slots = RING as u64 / SLOT;
    for run in &runs[1..] {
        assert!(u64::from(*run) >= slots / 4, "{runs:?}");
    }
}

#[test]
fn a_latest_start_overwritten_before_the_first_read_reports_the_loss() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    publish(&mut writer, 10);
    let start = Start::Latest {
        at: writer.position(),
        index: writer.next_index(),
    };
    let mut reader = reader_of(&heap, start);
    publish(&mut writer, 500);
    let Got::Lagged(lost) = take(&mut reader) else {
        panic!("the first read is not a loss");
    };
    let rest = drain(&mut reader);
    // Published after the subscription, less what was read.
    assert_eq!(lost, 500 - rest.len() as u64);
    assert_eq!(rest[0], item(10 + lost as u32));
    assert_eq!(rest.last(), Some(&item(509)));
}

#[test]
fn each_start_reads_its_first_record() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let latest_at = |writer: &Writer<_>| Start::Latest {
        at: writer.position(),
        index: writer.next_index(),
    };
    // On a topic without a record every start waits for the first one.
    let mut early = [Start::Oldest, Start::Last, latest_at(&writer)].map(|s| reader_of(&heap, s));
    for reader in &mut early {
        assert_eq!(take(reader), Got::Empty);
    }
    publish(&mut writer, 5);
    for reader in &mut early {
        assert_eq!(take(reader), item(0));
    }
    let mut oldest = reader_of(&heap, Start::Oldest);
    assert_eq!(take(&mut oldest), item(0));
    let mut last = reader_of(&heap, Start::Last);
    assert_eq!(take(&mut last), item(4));
    assert_eq!(take(&mut last), Got::Empty);
    let mut later = reader_of(&heap, latest_at(&writer));
    assert_eq!(take(&mut later), Got::Empty);
    publish(&mut writer, 3);
    assert_eq!(take(&mut later), item(5));
    assert_eq!(take(&mut last), item(5));
    // `Oldest` on a ring that has wrapped is `tail`: the ring keeps 128 of these records.
    publish(&mut writer, 300);
    let mut oldest = reader_of(&heap, Start::Oldest);
    assert_eq!(take(&mut oldest), item(308 - 128));

    let refused = |at| Reader::attach(heap.loaded(), Start::Latest { at, index: 0 }).err();
    let past = Some(Corrupted("start not a position the publisher committed"));
    assert_eq!(refused(writer.position() + 8), past);
    assert_eq!(refused(4), past);
    assert_eq!(refused(writer.position()), None);
}

#[test]
fn latest_returns_the_newest_record_and_reports_no_loss() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    assert_eq!(latest(&mut reader), Got::Empty);
    publish(&mut writer, 5);
    assert_eq!(latest(&mut reader), item(4));
    assert_eq!(latest(&mut reader), Got::Empty);
    assert_eq!(take(&mut reader), Got::Empty);
    // A reader the publisher has lapped several times over.
    publish(&mut writer, 1000);
    assert_eq!(latest(&mut reader), item(1004));
    assert_eq!(latest(&mut reader), Got::Empty);
    // What `latest` skipped is not a loss, to this call or to the next receive.
    publish(&mut writer, 2);
    assert_eq!(take(&mut reader), item(1005));
    assert_eq!(take(&mut reader), item(1006));
    assert_eq!(reader.lost(), 0);
}

#[test]
fn a_publisher_stopped_after_any_store_leaves_every_committed_record_readable() {
    for stop in [Stop::AfterRegionEnd, Stop::AfterWrite, Stop::AfterLast] {
        let heap = Heap::new(RING);
        let mut writer = writer_of(&heap);
        // Enough to wrap, so that the last write moves `tail` and `half` as well.
        publish(&mut writer, 300);
        let mut behind = reader_of(&heap, Start::Last);
        assert_eq!(take(&mut behind), item(299), "{stop:?}");
        writer.stop = stop;
        publish(&mut writer, 1);
        // The record the stopped write would have committed, when it got that far.
        let committed = if stop == Stop::AfterRegionEnd {
            300
        } else {
            301
        };

        let mut all = reader_of(&heap, Start::Oldest);
        let read = drain(&mut all);
        assert_eq!(read.last(), Some(&item(committed - 1)), "{stop:?}");
        assert!(read.len() >= 127, "{stop:?}: {}", read.len());

        // `last` is one record behind after `AfterWrite`; the newest is still reached.
        let mut newest = reader_of(&heap, Start::Oldest);
        let mut seen = Vec::new();
        while let Got::Item(index, payload) = latest(&mut newest) {
            assert_eq!(payload, body(index, BODY), "{stop:?}");
            seen.push(index);
        }
        assert_eq!(seen.last(), Some(&(committed - 1)), "{stop:?}");
        assert!(seen.len() <= 2, "{stop:?}: {seen:?}");

        // A reader that had read up to `last` when the publisher stopped.
        let expected = if committed == 301 {
            item(300)
        } else {
            Got::Empty
        };
        assert_eq!(latest(&mut behind), expected, "{stop:?}");
        assert_eq!(latest(&mut behind), Got::Empty, "{stop:?}");

        let mut lapped = reader_of(&heap, Start::Latest { at: 0, index: 0 });
        assert!(matches!(take(&mut lapped), Got::Lagged(_)), "{stop:?}");
        assert!(matches!(take(&mut lapped), Got::Item(..)), "{stop:?}");
    }
}

#[test]
fn the_end_record_is_the_last_and_every_call_returns_it() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 300);
    writer.put(Kind::End, b"closed").unwrap();
    assert_eq!(writer.put(Kind::Item, b""), Err(Refused::Ended));
    assert_eq!(writer.put(Kind::End, b""), Err(Refused::Ended));
    let read = drain(&mut reader);
    let end = Got::End(b"closed".to_vec());
    // Lapped before its first record, so nothing is reported: the newer half, then the end.
    assert_eq!(read[0], item(237));
    assert_eq!(read.iter().position(|got| *got == end), Some(63));
    assert_eq!(take(&mut reader), end);
    assert_eq!(latest(&mut reader), end);
    assert_eq!(take(&mut reader), end);
    // A reader that was lapped is told so first, and one that asks for the newest is not.
    let mut lapped = reader_of(&heap, Start::Latest { at: 0, index: 0 });
    assert_eq!(take(&mut lapped), Got::Lagged(237));
    assert_eq!(drain(&mut lapped)[63], end);
    let mut newest = reader_of(&heap, Start::Latest { at: 0, index: 0 });
    assert_eq!(latest(&mut newest), end);
    assert_eq!(take(&mut newest), end);
    assert_eq!(newest.lost(), 0);
}

#[test]
fn records_of_a_later_layout_are_stepped_over_without_a_loss() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 1);
    writer.put_raw(3, b"reserved").unwrap();
    writer.put_raw(2, &[]).unwrap();
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(0));
    assert_eq!(take(&mut reader), item(3));
    assert_eq!(reader.lost(), 0);
    // The newest record is one this version does not read: nothing to return, and it is consumed.
    writer.put_raw(3, &[0; 16]).unwrap();
    assert_eq!(latest(&mut reader), Got::Empty);
    assert_eq!(reader.position(), writer.position());
    // A ring of nothing else is crossed in one call.
    for _ in 0..100 {
        writer.put_raw(3, &[]).unwrap();
    }
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(105));
    assert_eq!(reader.lost(), 0);
}

/// A reader that has read record 0 of two, and a writer's view to forge the memory through.
fn forged() -> (Heap, Reader<impl Loads>, impl Stores) {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    publish(&mut writer, 2);
    let mut reader = reader_of(&heap, Start::Oldest);
    assert_eq!(take(&mut reader), item(0));
    let forge = heap.stored();
    (heap, reader, forge)
}

/// The reader fails with `what`, and goes on failing with it whatever is asked.
fn assert_corrupted<M: Loads>(reader: &mut Reader<M>, what: &'static str) {
    assert_eq!(take(reader), Got::Corrupted(what));
    assert_eq!(latest(reader), Got::Corrupted(what));
    assert_eq!(take(reader), Got::Corrupted(what));
}

#[test]
fn a_forged_length_ends_the_reader_without_a_copy_outside_the_ring() {
    let header = |len| {
        let header = Header {
            kind: 0,
            len,
            index: 1,
        };
        header.bytes()
    };
    let (_heap, mut reader, forge) = forged();
    forge.store_ring(SLOT, &header(2041));
    assert_corrupted(&mut reader, "record longer than half the ring");

    let (_heap, mut reader, forge) = forged();
    forge.store_ring(SLOT, &header((1 << 30) - 1));
    assert_corrupted(&mut reader, "record longer than half the ring");

    // A length the ring could hold, reaching past what is committed.
    let (_heap, mut reader, forge) = forged();
    forge.store_ring(SLOT, &header(BODY as u32 + 1));
    assert_corrupted(&mut reader, "record reaches past the write counter");

    // The same from the call that reads the newest record.
    let (_heap, mut reader, forge) = forged();
    forge.store_ring(SLOT, &header(2041));
    assert_eq!(
        latest(&mut reader),
        Got::Corrupted("record longer than half the ring")
    );
}

#[test]
fn forged_counters_end_the_reader() {
    let (_heap, mut reader, forge) = forged();
    forge.store_tail(2 * SLOT + 8);
    assert_corrupted(&mut reader, "tail past the write counter");

    // A `write` is loaded again once the reader reaches the one it holds: after record 1.
    let (_heap, mut reader, forge) = forged();
    forge.store_write(RING as u64 + 8);
    assert_eq!(take(&mut reader), item(1));
    assert_corrupted(&mut reader, "more bytes kept than the ring holds");

    let (_heap, mut reader, forge) = forged();
    forge.store_write(SLOT);
    assert_eq!(take(&mut reader), item(1));
    assert_corrupted(&mut reader, "write counter moved backwards");

    let (_heap, mut reader, forge) = forged();
    forge.store_tail(SLOT + 4);
    assert_corrupted(&mut reader, "tail off the record grid");

    let (_heap, mut reader, forge) = forged();
    forge.store_write(2 * SLOT + 4);
    assert_eq!(take(&mut reader), item(1));
    assert_corrupted(&mut reader, "write counter off the record grid");

    // The call for the newest record loads `write` every time.
    let (_heap, mut reader, forge) = forged();
    forge.store_write(SLOT);
    assert_eq!(
        latest(&mut reader),
        Got::Corrupted("write counter moved backwards")
    );

    // `half` is read by a lapped reader, `last` by one that wants the newest record.
    for half in [SLOT, 3 * SLOT, 2 * SLOT + 4] {
        let (_heap, mut reader, forge) = forged();
        forge.store_write(3 * SLOT);
        forge.store_tail(2 * SLOT);
        forge.store_half(half);
        assert_corrupted(&mut reader, "half outside the kept records");
    }
    for last in [3 * SLOT, 2 * SLOT + 4] {
        let (_heap, mut reader, forge) = forged();
        forge.store_write(3 * SLOT);
        forge.store_last(last);
        assert_eq!(
            latest(&mut reader),
            Got::Corrupted("last outside the kept records")
        );
        assert_eq!(
            take(&mut reader),
            Got::Corrupted("last outside the kept records")
        );
    }
    let (_heap, mut reader, forge) = forged();
    forge.store_tail(2 * SLOT);
    forge.store_write(3 * SLOT);
    forge.store_last(SLOT);
    assert_eq!(
        latest(&mut reader),
        Got::Corrupted("last outside the kept records")
    );
}

/// A reader's view behind which a publisher laps the reader each time `tail` or `write` is loaded.
struct Chased<'a, M, S> {
    memory: M,
    writer: &'a RefCell<Writer<S>>,
    chasing: &'a Cell<bool>,
    halves: &'a Cell<u32>,
    copies: &'a Cell<u32>,
}

impl<M: Loads, S: Stores> Loads for Chased<'_, M, S> {
    fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    fn tail(&self) -> u64 {
        self.chase();
        self.memory.tail()
    }

    fn write(&self) -> u64 {
        self.chase();
        self.memory.write()
    }

    fn last(&self) -> u64 {
        self.memory.last()
    }

    fn half(&self) -> u64 {
        self.halves.set(self.halves.get() + 1);
        self.memory.half()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        self.copies.set(self.copies.get() + 1);
        self.memory.load_ring(position, dst);
    }

    fn fence(&self) {
        self.memory.fence();
    }
}

impl<M, S: Stores> Chased<'_, M, S> {
    /// More than a ring's worth, before each counter the reader loads that could tell it so.
    fn chase(&self) {
        if self.chasing.get() {
            publish(&mut self.writer.borrow_mut(), 200);
        }
    }
}

#[test]
fn a_publisher_that_laps_every_look_does_not_hold_a_call() {
    let heap = Heap::new(RING);
    let writer = RefCell::new(writer_of(&heap));
    let (chasing, halves, copies) = (Cell::new(false), Cell::new(0), Cell::new(0));
    let chased = Chased {
        memory: heap.loaded(),
        writer: &writer,
        chasing: &chasing,
        halves: &halves,
        copies: &copies,
    };
    publish(&mut writer.borrow_mut(), 1);
    let mut reader = Reader::attach(chased, Start::Oldest).unwrap();
    assert_eq!(take(&mut reader), item(0));

    chasing.set(true);
    halves.set(0);
    copies.set(0);
    // Lapped at every look, the call resumes `RETRIES` times and returns: no record was whole.
    assert_eq!(take(&mut reader), Got::Empty);
    assert_eq!(halves.get(), RETRIES);
    assert!(copies.get() <= 2 * (RETRIES + 1), "{}", copies.get());
    copies.set(0);
    assert_eq!(latest(&mut reader), Got::Empty);
    assert!(copies.get() <= 2 * (RETRIES + 1), "{}", copies.get());
    assert_eq!(reader.lost(), 0);

    // Once the publisher rests, the loss is reported in full and reading goes on.
    chasing.set(false);
    let published = writer.borrow().next_index();
    let Got::Lagged(lost) = take(&mut reader) else {
        panic!("the loss was not reported");
    };
    let rest = drain(&mut reader);
    assert_eq!(rest[0], item(1 + lost as u32));
    assert_eq!(1 + lost + rest.len() as u64, u64::from(published));
}

/// A reader's view behind which a publisher writes more than a ring at the `tail` load `at`.
struct Lapping<'a, M, S> {
    memory: M,
    writer: &'a RefCell<Writer<S>>,
    tails: Cell<u32>,
    at: u32,
}

impl<M: Loads, S: Stores> Loads for Lapping<'_, M, S> {
    fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    fn tail(&self) -> u64 {
        self.tails.set(self.tails.get() + 1);
        if self.tails.get() == self.at {
            publish(&mut self.writer.borrow_mut(), 200);
        }
        self.memory.tail()
    }

    fn write(&self) -> u64 {
        self.memory.write()
    }

    fn last(&self) -> u64 {
        self.memory.last()
    }

    fn half(&self) -> u64 {
        self.memory.half()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        self.memory.load_ring(position, dst);
    }

    fn fence(&self) {
        self.memory.fence();
    }
}

#[test]
fn an_oldest_start_whose_tail_passed_the_write_counter_it_loaded_reads_on() {
    let heap = Heap::new(RING);
    let writer = RefCell::new(writer_of(&heap));
    publish(&mut writer.borrow_mut(), 128);
    // Attach loads `tail`, `write`, `tail`: the second `tail` is past the `write` before it.
    let lapping = Lapping {
        memory: heap.loaded(),
        writer: &writer,
        tails: Cell::new(0),
        at: 2,
    };
    let mut reader = Reader::attach(lapping, Start::Oldest).unwrap();
    assert_eq!(take(&mut reader), item(200));
    assert_eq!(drain(&mut reader).len(), 127);
}

/// What the two sides of the race below share to pace one another.
struct Pace {
    /// Ring stores the publisher has made.
    stores: AtomicU32,
    /// The publisher has written its last record.
    done: AtomicBool,
}

/// Where a reader behind [`Paced`] memory lets the publisher run.
#[derive(Clone, Copy, PartialEq)]
enum Rhythm {
    /// Here and there, for a few stores: before a copy, and at times between a copy and its check.
    Scattered,
    /// Before each payload copy, for the next of [`STAGES`] stores.
    Staged,
}

/// Ring stores a staged reader waits for. A record is two; four or more overwrite its record.
const STAGES: [u32; 6] = [0, 4, 1, 5, 2, 6];

/// Memory behind which a reader lets the publisher store a while between two of its own steps.
struct Paced<'a, M>(M, &'a Pace, Cell<u32>, Rhythm);

impl<M> Paced<'_, M> {
    /// Wait until the publisher has made `stores` more ring stores, or its last.
    fn wait(&self, stores: u32) {
        // `Relaxed`: an acquire would order the reader after the publisher's stores to the ring.
        let until = self.1.stores.load(Ordering::Relaxed) + stores;
        while self.1.stores.load(Ordering::Relaxed) < until && !self.1.done.load(Ordering::Relaxed)
        {
            std::thread::yield_now();
        }
    }

    /// The next of a fixed sequence of numbers below `below`, for a scattered reader's waits.
    fn scatter(&self, below: u32) -> u32 {
        let state = self.2.get().wrapping_mul(1103515245).wrapping_add(12345);
        self.2.set(state);
        (state >> 16) % below
    }
}

impl<M: Stores> Stores for Paced<'_, M> {
    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn store_tail(&self, position: u64) {
        self.0.store_tail(position);
    }

    fn store_region_end(&self, position: u64) {
        self.0.store_region_end(position);
    }

    fn store_write(&self, position: u64) {
        self.0.store_write(position);
    }

    fn store_last(&self, position: u64) {
        self.0.store_last(position);
    }

    fn store_half(&self, position: u64) {
        self.0.store_half(position);
    }

    fn bump_seq(&self) -> u32 {
        self.0.bump_seq()
    }

    fn wake_seq(&self) {
        self.0.wake_seq();
    }

    fn store_ring(&self, position: u64, src: &[u8]) {
        self.0.store_ring(position, src);
        self.1.stores.fetch_add(1, Ordering::Relaxed);
        std::thread::yield_now();
    }
}

impl<M: Loads> Loads for Paced<'_, M> {
    fn capacity(&self) -> u64 {
        self.0.capacity()
    }

    fn tail(&self) -> u64 {
        self.0.tail()
    }

    fn write(&self) -> u64 {
        self.0.write()
    }

    fn last(&self) -> u64 {
        self.0.last()
    }

    fn half(&self) -> u64 {
        self.0.half()
    }

    fn load_ring(&self, position: u64, dst: &mut [u8]) {
        // Between a look and the copy: at times the publisher writes a ring's worth there.
        if self.3 == Rhythm::Scattered {
            // Mostly up to three stores; one time in four, enough to write the whole ring anew.
            let turn = self.scatter(16);
            self.wait(if turn < 12 { turn % 4 } else { turn });
        }
        if self.3 == Rhythm::Staged && dst.len() > 8 {
            let turn = self.2.replace(self.2.get() + 1) as usize;
            self.wait(STAGES[turn % STAGES.len()]);
        }
        self.0.load_ring(position, dst);
    }

    fn fence(&self) {
        // Between a copy and the load of `tail` that judges it: a publish may fall between them.
        if self.3 == Rhythm::Scattered {
            // One or two stores, in two of every 48 judgments.
            self.wait(Some(self.scatter(48)).filter(|n| *n < 3).unwrap_or(0));
        }
        self.0.fence();
    }
}

/// Sets the flag when dropped.
struct Finished<'a>(&'a AtomicBool);

impl Drop for Finished<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

/// A publisher of `records` records, a ring of two, a reader of each kind: how many the first read.
fn race(rhythm: Rhythm, records: u32, len_of: fn(u32) -> usize) -> u32 {
    let heap = Heap::new(64);
    let pace = Pace {
        stores: AtomicU32::new(0),
        done: AtomicBool::new(false),
    };
    let done = &pace.done;
    let paced = |memory, seed| Paced(memory, &pace, Cell::new(seed), rhythm);
    let mut writer = Writer::new(Paced(heap.stored(), &pace, Cell::new(0), rhythm));
    let attach = |seed| {
        let from_start = Start::Latest { at: 0, index: 0 };
        Reader::attach(paced(heap.loaded(), seed), from_start).unwrap()
    };
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Set when the thread ends, by a panic too: the readers' pauses wait for it.
            let _finished = Finished(done);
            for index in 0..records {
                let payload = body(index, len_of(index));
                writer.put(Kind::Item, &payload).unwrap();
            }
        });
        scope.spawn(|| {
            let mut newest = attach(2);
            let mut above = 0;
            while !done.load(Ordering::Acquire) {
                match latest(&mut newest) {
                    Got::Item(index, payload) => {
                        assert_eq!(payload, body(index, len_of(index)), "record {index}");
                        assert!(index >= above, "{index} after {above}");
                        above = index + 1;
                    }
                    Got::Empty => std::thread::yield_now(),
                    other => panic!("newest: {other:?}"),
                }
            }
        });
        let mut reader = attach(1);
        let (mut next, mut read) = (0u32, 0u32);
        loop {
            let finished = done.load(Ordering::Acquire);
            match take(&mut reader) {
                Got::Item(index, payload) => {
                    assert_eq!(payload, body(index, len_of(index)), "record {index}");
                    assert_eq!(index, next, "an index out of order");
                    (next, read) = (next + 1, read + 1);
                }
                Got::Lagged(lost) => next += lost as u32,
                // Empty with the publisher done before the call: nothing more will come.
                Got::Empty if finished => break,
                Got::Empty => std::thread::yield_now(),
                other => panic!("{other:?}"),
            }
        }
        // Every record was either returned or reported as lost, and none twice.
        assert_eq!(next, records);
        assert_eq!(u64::from(read) + reader.lost(), u64::from(records));
        read
    })
}

// Under Miri the seeds of `scripts/local_gate.sh` decide where the threads interleave.
#[test]
fn records_returned_while_the_publisher_overwrites_are_whole() {
    let records = if cfg!(miri) { 120 } else { 100_000 };
    // Records of 8, 16 and 32 bytes, one of them with padding.
    let read = race(Rhythm::Scattered, records, |index| {
        [0, 5, 24][index as usize % 3]
    });
    assert!(read > 0);
}

// What tells `Release` ring stores from `Relaxed` ones, under Miri: x86 runs both as one code.
#[test]
fn a_copy_that_caught_an_overwrite_is_judged_by_a_tail_stored_before_it() {
    let records = if cfg!(miri) { 120 } else { 30_000 };
    // Each payload copy comes after a wait that leaves the record whole, or does not.
    let read = race(Rhythm::Staged, records, |_| BODY);
    // Some whole record returned, so both judgements ran; how many depends on the machine's load.
    assert!(read > 0, "{read} of {records}");
}

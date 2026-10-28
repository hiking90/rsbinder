// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The writer and the reader over heap memory, which is what Miri can run (it has no memfd and
//! no `mmap`). The mapped memory is exercised by `mapped_tests.rs`.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
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
fn a_record_of_parts_is_their_bytes_then_zeros_to_a_word() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    // The second part completes a word the first began, the third begins the last one.
    let parts: [&[u8]; 4] = [b"abc", b"defgh", b"", b"ij"];
    writer.put_parts(Kind::Item, 10, parts).unwrap();
    let mut stored = [0xff; 16];
    heap.loaded().load_ring(rules::HEADER, &mut stored);
    assert_eq!(&stored, b"abcdefghij\0\0\0\0\0\0");
    assert_eq!(
        take(&mut reader_of(&heap, Start::Oldest)),
        Got::Item(0, b"abcdefghij".to_vec())
    );
}

#[test]
fn a_loan_record_is_returned_with_what_it_names_between_items() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    let loan = Loan {
        slot: 2,
        len: 9,
        generation: 4,
    };
    publish(&mut writer, 1);
    writer.put(Kind::Loan, &loan.bytes()).unwrap();
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(0));
    assert_eq!(take(&mut reader), Got::Loan(1, loan));
    assert_eq!(take(&mut reader), item(2));
    writer.put(Kind::Loan, &loan.bytes()).unwrap();
    assert_eq!(latest(&mut reader), Got::Loan(3, loan));
    assert_eq!(reader.position(), writer.position());
    // One of another length is no loan of this layout's.
    writer.put(Kind::Loan, &[0; 8]).unwrap();
    assert_eq!(
        take(&mut reader),
        Got::Corrupted("loan record of another length")
    );
}

#[test]
fn records_of_a_later_layout_are_stepped_over_without_a_loss() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 1);
    writer.put_raw(3, 8, [&b"reserved"[..]]).unwrap();
    writer.put_raw(3, 0, []).unwrap();
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), item(0));
    assert_eq!(take(&mut reader), item(3));
    assert_eq!(reader.lost(), 0);
    // The newest record is one this version does not read: nothing to return, and it is consumed.
    writer.put_raw(3, 16, [&[0; 16][..]]).unwrap();
    assert_eq!(latest(&mut reader), Got::Empty);
    assert_eq!(reader.position(), writer.position());
    // A ring of nothing else is crossed in one call.
    for _ in 0..100 {
        writer.put_raw(3, 0, []).unwrap();
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

/// The reader fails with `what` from the newest-record call (it loads `tail` unlapped), then again.
fn assert_corrupted_newest<M: Loads>(reader: &mut Reader<M>, what: &'static str) {
    assert_eq!(latest(reader), Got::Corrupted(what));
    assert_eq!(take(reader), Got::Corrupted(what));
}

#[test]
fn forged_counters_end_the_reader() {
    let (_heap, mut reader, forge) = forged();
    forge.store_tail(2 * SLOT + 8);
    assert_corrupted_newest(&mut reader, "tail past the write counter");

    // `write` is loaded again once the reader reaches it (after record 1), below `frontier`.
    let (_heap, mut reader, forge) = forged();
    forge.store_write(RING as u64 + 8);
    assert_eq!(take(&mut reader), item(1));
    assert_corrupted(&mut reader, "frontier behind the write counter");

    // `frontier` judges each copy.
    let (_heap, mut reader, forge) = forged();
    forge.store_frontier(SLOT);
    assert_corrupted(&mut reader, "frontier moved backwards");

    let (_heap, mut reader, forge) = forged();
    forge.store_frontier(2 * SLOT + 4);
    assert_corrupted(&mut reader, "frontier off the record grid");

    let (_heap, mut reader, forge) = forged();
    forge.store_write(SLOT);
    assert_eq!(take(&mut reader), item(1));
    assert_corrupted(&mut reader, "write counter moved backwards");

    let (_heap, mut reader, forge) = forged();
    forge.store_tail(SLOT + 4);
    assert_corrupted_newest(&mut reader, "tail off the record grid");

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
        // Past record 1 by a ring: the reader takes itself for lapped there.
        forge.store_frontier(RING as u64 + 2 * SLOT);
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

/// A reader's view where a publisher laps the reader at each load of `tail`, `frontier`, `write`.
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

    fn frontier(&self) -> u64 {
        self.chase();
        self.memory.frontier()
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

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        self.copies.set(self.copies.get() + 1);
        self.memory.load_ring_extend(position, len, dst);
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

    fn frontier(&self) -> u64 {
        self.memory.frontier()
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

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        self.memory.load_ring_extend(position, len, dst);
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

/// A reader's view where `publish` writes at its `frontier` load `at`, counting ring bytes loaded.
struct Overtaking<'a, M, S> {
    memory: M,
    writer: &'a RefCell<Writer<S>>,
    publish: fn(&mut Writer<S>),
    looks: Cell<u32>,
    at: Cell<u32>,
    loaded: Cell<usize>,
}

impl<M: Loads, S: Stores> Loads for Overtaking<'_, M, S> {
    fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    fn tail(&self) -> u64 {
        self.memory.tail()
    }

    fn frontier(&self) -> u64 {
        self.looks.set(self.looks.get() + 1);
        if self.looks.get() == self.at.get() {
            (self.publish)(&mut self.writer.borrow_mut());
        }
        self.memory.frontier()
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
        self.loaded.set(self.loaded.get() + dst.len());
        self.memory.load_ring(position, dst);
    }

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        self.loaded.set(self.loaded.get() + len);
        self.memory.load_ring_extend(position, len, dst);
    }

    fn fence(&self) {
        self.memory.fence();
    }
}

#[test]
fn a_copy_the_publisher_overtakes_stops_at_the_next_chunk() {
    const LONG: usize = 16 * CHUNK;
    let heap = Heap::new(RING);
    let writer = RefCell::new(writer_of(&heap));
    publish(&mut writer.borrow_mut(), 1);
    let overtaking = Overtaking {
        memory: heap.loaded(),
        writer: &writer,
        publish: |writer| publish(writer, 200),
        looks: Cell::new(0),
        at: Cell::new(0),
        loaded: Cell::new(0),
    };
    let mut reader = Reader::attach(overtaking, Start::Oldest).unwrap();
    // A record overtaken before the first one read would not count as lost.
    assert_eq!(take(&mut reader), item(0));
    writer.borrow_mut().put(Kind::Item, &body(1, LONG)).unwrap();
    // The first `frontier` the receive loads is the one after its first chunk.
    reader.memory.at.set(reader.memory.looks.get() + 1);
    reader.memory.loaded.set(0);
    let Got::Lagged(lost) = take(&mut reader) else {
        panic!("the overtaken record was not reported lost");
    };
    // Its header and first chunk, then the header of the record it resumes at.
    let header = rules::HEADER as usize;
    assert_eq!(reader.memory.loaded.get(), header + CHUNK + header);
    let rest = drain(&mut reader);
    assert_eq!(rest[0], item(1 + lost as u32));
    assert_eq!(
        1 + lost + rest.len() as u64,
        u64::from(writer.borrow().next_index())
    );
}

/// Of two half-ring records, the first's head is overwritten by `len` bytes once two chunks copy.
fn written_over_behind_the_reader(len: usize) -> Got {
    const HALF: usize = 128 - rules::HEADER as usize;
    let heap = Heap::new(256);
    let writer = RefCell::new(writer_of(&heap));
    for index in 0..2 {
        writer
            .borrow_mut()
            .put(Kind::Item, &body(index, HALF))
            .unwrap();
    }
    let behind: fn(&mut Writer<_>) = match len {
        8 => |writer| writer.put(Kind::Item, &body(2, 8)).unwrap(),
        16 => |writer| writer.put(Kind::Item, &body(2, 16)).unwrap(),
        _ => |writer| writer.put(Kind::Item, &body(2, 24)).unwrap(),
    };
    let overtaking = Overtaking {
        memory: heap.loaded(),
        writer: &writer,
        publish: behind,
        looks: Cell::new(0),
        // The look that judges the second chunk, from 8 + CHUNK on; the first passed its own.
        at: Cell::new(2),
        loaded: Cell::new(0),
    };
    let mut reader = Reader::attach(overtaking, Start::Oldest).unwrap();
    take(&mut reader)
}

#[test]
fn a_record_written_over_behind_the_reader_is_returned_while_its_rest_is_not() {
    const HALF: usize = 128 - rules::HEADER as usize;
    let whole = Got::Item(0, body(0, HALF));
    // Up to where the second chunk starts, the reader copied it before the publisher came.
    assert_eq!(written_over_behind_the_reader(8), whole);
    assert_eq!(written_over_behind_the_reader(16), whole);
    // A word into the second chunk: judged as written over, and the reader resumes at `half`.
    assert_eq!(
        written_over_behind_the_reader(24),
        Got::Item(2, body(2, 24))
    );
}

/// A view where a lap is written inside the header load at `at`, which returns what `forge` makes.
struct Forging<M, S> {
    memory: M,
    writer: Rc<RefCell<Writer<S>>>,
    at: Cell<Option<u64>>,
    forge: fn([u8; 8], [u8; 8]) -> [u8; 8],
}

impl<M: Loads, S: Stores> Loads for Forging<M, S> {
    fn capacity(&self) -> u64 {
        self.memory.capacity()
    }

    fn tail(&self) -> u64 {
        self.memory.tail()
    }

    fn frontier(&self) -> u64 {
        self.memory.frontier()
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
        if dst.len() != 8 || self.at.get() != Some(position) {
            self.memory.load_ring(position, dst);
            return;
        }
        self.at.set(None);
        let (mut before, mut after) = ([0; 8], [0; 8]);
        self.memory.load_ring(position, &mut before);
        // More than a ring: past any end a drain before it took, and the bytes loaded too.
        publish(&mut self.writer.borrow_mut(), 200);
        self.memory.load_ring(position, &mut after);
        dst.copy_from_slice(&(self.forge)(before, after));
    }

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        if len != 8 || self.at.get() != Some(position) {
            self.memory.load_ring_extend(position, len, dst);
            return;
        }
        let start = dst.len();
        dst.resize(start + len, 0);
        self.load_ring(position, &mut dst[start..]);
    }

    fn fence(&self) {
        self.memory.fence();
    }
}

/// A drain to the end of ten records, `read` read, lapped at the next header: the call, the reader.
fn forged_at_the_next_header(
    read: u32,
    forge: fn([u8; 8], [u8; 8]) -> [u8; 8],
) -> (Got, Reader<impl Loads>) {
    let heap = Heap::new(RING);
    let writer = Rc::new(RefCell::new(writer_of(&heap)));
    publish(&mut writer.borrow_mut(), 10);
    let forging = Forging {
        memory: heap.loaded(),
        writer: writer.clone(),
        at: Cell::new(None),
        forge,
    };
    let mut reader = Reader::attach(forging, Start::Oldest).unwrap();
    for index in 0..read {
        assert_eq!(take(&mut reader), item(index));
    }
    reader.stop_at(Some(writer.borrow().position()));
    reader.memory.at.set(Some(reader.position()));
    let got = take(&mut reader);
    (got, reader)
}

/// After a forged header: the reader read on as a live one reads only whole records it published.
fn reads_on_honestly<M: Loads>(reader: &mut Reader<M>, read: u32, published: u32) {
    reader.stop_at(None);
    let lost_before = reader.lost();
    let rest = drain(reader);
    let Some(Got::Item(first, _)) = rest.iter().find(|got| matches!(got, Got::Item(..))) else {
        panic!("nothing read on: {rest:?}");
    };
    let items: Vec<_> = rest
        .iter()
        .filter(|got| matches!(got, Got::Item(..)))
        .collect();
    let expected: Vec<_> = (*first..published).map(item).collect();
    assert_eq!(items, expected.iter().collect::<Vec<_>>());
    if read > 0 {
        // Every record from the first read on returned or counted, and none twice.
        assert_eq!(
            u64::from(read) + reader.lost() + items.len() as u64,
            u64::from(published)
        );
    } else {
        assert_eq!(reader.lost(), lost_before);
    }
}

#[test]
fn an_overtaken_copy_never_moves_the_reader_by_its_header() {
    // A word of zeros, as a payload of zeros of the new lap would be, before any record is read.
    let (got, mut reader) = forged_at_the_next_header(0, |_, _| [0; 8]);
    assert_eq!(
        (got, reader.gave_up(), reader.position()),
        (Got::Empty, true, 0)
    );
    assert_eq!(reader.count_lost_before(), Ok(0));
    reads_on_honestly(&mut reader, 0, 210);

    // Torn as a 32-bit reader's two loads can tear it: the new first word, the old index.
    let torn = |before: [u8; 8], after: [u8; 8]| {
        let mut torn = after;
        torn[4..].copy_from_slice(&before[4..]);
        torn
    };
    let (got, mut reader) = forged_at_the_next_header(2, torn);
    assert_eq!(
        (got, reader.gave_up(), reader.position()),
        (Got::Empty, true, 2 * SLOT)
    );
    assert_eq!(reader.lost(), 0);
    // Item 2 lies before the end, which was written over too: a lower bound.
    assert_eq!(reader.count_lost_before(), Ok(1));
    reads_on_honestly(&mut reader, 2, 210);
}

/// A payload of three words, each the header of item 2 with no payload.
fn looks_like_item_2() -> Vec<u8> {
    let word = Header {
        kind: 0,
        len: 0,
        index: 2,
    };
    word.bytes().repeat(3)
}

#[test]
fn a_payload_word_that_looks_like_the_header_expected_moves_the_reader_nowhere() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 10);
    assert_eq!(take(&mut reader), item(0));
    assert_eq!(take(&mut reader), item(1));
    reader.stop_at(Some(writer.position()));
    // Shifted by 16, the new lap puts a payload's middle word, item 2's header, at item 2's start.
    writer.put(Kind::Item, &body(10, 8)).unwrap();
    for _ in 0..200 {
        writer.put(Kind::Item, &looks_like_item_2()).unwrap();
    }
    let at = reader.position();
    let mut header = [0; 8];
    heap.loaded().load_ring(at, &mut header);
    assert_eq!(Header::parse(header).index, 2);
    assert_eq!(take(&mut reader), Got::Empty);
    assert_eq!((reader.gave_up(), reader.position()), (true, at));
    assert_eq!(reader.count_lost_before(), Ok(1));
    // Read on: whole records only, the ones of the new lap as they were published.
    reader.stop_at(None);
    let Got::Lagged(_) = take(&mut reader) else {
        panic!("the loss was not reported");
    };
    for got in drain(&mut reader) {
        let Got::Item(index, payload) = got else {
            panic!("{got:?}");
        };
        assert_eq!(payload, looks_like_item_2(), "item {index}");
    }
    assert_eq!(reader.position(), writer.position());
}

/// Record 1 of three read, lapped by `frontier`, `tail` and `half` at `at`; record 2 is `third`.
fn lapped_onto(at: u64, frontier: u64, third: u32) -> Got {
    let (_heap, mut reader, forge) = forged();
    let header = Header {
        kind: 0,
        len: BODY as u32,
        index: third,
    };
    forge.store_ring(2 * SLOT, &header.bytes());
    forge.store_ring(2 * SLOT + 8, &body(third, BODY));
    forge.store_write(3 * SLOT);
    forge.store_tail(at);
    forge.store_half(at);
    forge.store_frontier(frontier);
    take(&mut reader)
}

/// [`lapped_onto`] a third record of the index that follows.
fn lapped_with_tail(at: u64, frontier: u64) -> Got {
    lapped_onto(at, frontier, 2)
}

#[test]
fn a_drain_at_its_end_or_before_its_first_record_counts_nothing() {
    let heap = Heap::new(RING);
    let (mut writer, mut reader) = (writer_of(&heap), reader_of(&heap, Start::Oldest));
    publish(&mut writer, 3);
    assert_eq!(reader.count_lost_before(), Ok(0), "not stopped");
    reader.stop_at(Some(writer.position()));
    assert_eq!(reader.count_lost_before(), Ok(0), "no record read yet");
    assert_eq!(drain(&mut reader), (0..3).map(item).collect::<Vec<_>>());
    // At the end: whatever lies there, nothing before it is owed.
    assert_eq!(reader.count_lost_before(), Ok(0), "at the end");
    publish(&mut writer, 1);
    assert_eq!(
        reader.count_lost_before(),
        Ok(0),
        "at the end, committed now"
    );
    assert_eq!((reader.lost(), reader.position()), (0, 3 * SLOT));
}

/// The record owed found whole where a lap resumes, as only a broken publisher puts it: returned.
#[test]
fn a_record_owed_found_where_a_lap_resumes_is_returned_by_that_call() {
    assert_eq!(lapped_onto(2 * SLOT, 2 * SLOT + RING as u64, 1), item(1));
}

#[test]
fn a_lap_resumes_ahead_of_the_reader_or_the_reader_fails() {
    let what = Got::Corrupted("frontier more than a ring past tail");
    // `tail` at the reader, which a `frontier` a ring past it does not allow.
    assert_eq!(lapped_with_tail(SLOT, SLOT + RING as u64 + 8), what);
    // Ahead of the reader, yet more than a ring behind `frontier`.
    assert_eq!(lapped_with_tail(2 * SLOT, 2 * SLOT + RING as u64 + 8), what);
    // A ring behind it: the reader resumes there, record 1 lost.
    assert_eq!(
        lapped_with_tail(2 * SLOT, 2 * SLOT + RING as u64),
        Got::Lagged(1)
    );
}

#[test]
fn a_drain_resumes_at_tail_and_a_live_reader_at_half() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut live, mut stopped) = (
        reader_of(&heap, Start::Oldest),
        reader_of(&heap, Start::Oldest),
    );
    publish(&mut writer, 1);
    assert_eq!(take(&mut live), item(0));
    assert_eq!(take(&mut stopped), item(0));
    publish(&mut writer, 200);
    stopped.stop_at(Some(writer.position()));
    let memory = heap.loaded();
    let (tail, half) = ((memory.tail() / SLOT) as u32, (memory.half() / SLOT) as u32);
    assert!(tail < half, "{tail} {half}");
    assert_eq!(take(&mut stopped), Got::Lagged(u64::from(tail) - 1));
    assert_eq!(take(&mut live), Got::Lagged(u64::from(half) - 1));
    // The publisher wrote nothing after the lap: every record from `tail` on, none counted.
    assert_eq!(
        drain(&mut stopped),
        (tail..201).map(item).collect::<Vec<_>>()
    );
    assert_eq!(drain(&mut live), (half..201).map(item).collect::<Vec<_>>());
    let lost = (u64::from(tail) - 1, u64::from(half) - 1);
    assert_eq!((stopped.lost(), live.lost()), lost);
}

/// Item 0 read, 200 records, a drain to their end resumed at `tail`: the reader, end, `tail`.
fn lapped_drain(heap: &Heap, writer: &mut Writer<impl Stores>) -> (Reader<impl Loads>, u64, u32) {
    let mut reader = reader_of(heap, Start::Oldest);
    publish(writer, 1);
    assert_eq!(take(&mut reader), item(0));
    publish(writer, 200);
    let end = writer.position();
    reader.stop_at(Some(end));
    let tail = (heap.loaded().tail() / SLOT) as u32;
    assert_eq!(take(&mut reader), Got::Lagged(u64::from(tail) - 1));
    (reader, end, tail)
}

/// Every receive of `reader` until it reaches `end`, `before_each` run before each.
fn until_end<M: Loads>(
    reader: &mut Reader<M>,
    end: u64,
    mut before_each: impl FnMut(),
) -> Vec<Got> {
    let mut returned = Vec::new();
    while reader.position() < end {
        before_each();
        returned.push(take(reader));
    }
    returned
}

/// A drain at `tail` whose record there is written over once: it resumes at `tail` again.
#[test]
fn a_drain_overtaken_once_where_it_resumed_at_tail_resumes_at_tail_again() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut reader, end, tail) = lapped_drain(&heap, &mut writer);
    // One record writes over the record at `tail`; the publisher stops.
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), Got::Lagged(1));
    let returned = until_end(&mut reader, end, || {});
    assert_eq!(returned, (tail + 1..201).map(item).collect::<Vec<_>>());
    assert_eq!(1 + reader.lost() + returned.len() as u64, 201);
}

/// One record before each of two receives of a drain at `tail`, each writing over it: `half`.
fn turned<M: Loads>(
    heap: &Heap,
    writer: &mut Writer<impl Stores>,
    reader: &mut Reader<M>,
    tail: u32,
) -> u32 {
    publish(writer, 1);
    assert_eq!(take(reader), Got::Lagged(1));
    publish(writer, 1);
    let half = (heap.loaded().half() / SLOT) as u32;
    assert_eq!(take(reader), Got::Lagged(u64::from(half - tail - 1)));
    half
}

/// A drain at `tail` whose record there is written over twice in a row: it resumes at `half`.
#[test]
fn a_drain_overtaken_twice_where_it_resumed_at_tail_resumes_at_half() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut reader, end, tail) = lapped_drain(&heap, &mut writer);
    // One record before each receive from now on.
    let half = turned(&heap, &mut writer, &mut reader, tail);
    let returned = until_end(&mut reader, end, || publish(&mut writer, 1));
    assert_eq!(returned, (half..201).map(item).collect::<Vec<_>>());
    assert_eq!(1 + reader.lost() + returned.len() as u64, 201);
}

/// A drain overtaken once at `tail`, then lapped after it read on: the count starts again.
#[test]
fn a_drain_that_read_on_between_two_overtakes_at_tail_resumes_at_tail() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut reader, _, tail) = lapped_drain(&heap, &mut writer);
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), Got::Lagged(1));
    assert_eq!(take(&mut reader), item(tail + 1));
    // Two records write over the next one, and `tail` is the one after it.
    publish(&mut writer, 2);
    assert_eq!(heap.loaded().tail() / SLOT, u64::from(tail + 3));
    assert_eq!(take(&mut reader), Got::Lagged(1));
    // Overtaken where that resume put it: the first of a new count, at `tail` again.
    publish(&mut writer, 1);
    assert_eq!(take(&mut reader), Got::Lagged(1));
    assert_eq!(take(&mut reader), item(tail + 4));
}

/// A drain that read on from where it resumed at `tail`, lapped again: it resumes at `tail` again.
#[test]
fn a_drain_lapped_after_it_read_on_from_tail_resumes_at_tail_again() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut reader, _, tail) = lapped_drain(&heap, &mut writer);
    for index in tail..tail + 3 {
        assert_eq!(take(&mut reader), item(index));
    }
    // Four records write over the next one, and `tail` is the one after it.
    publish(&mut writer, 4);
    assert_eq!(heap.loaded().tail() / SLOT, u64::from(tail + 4));
    assert_eq!(take(&mut reader), Got::Lagged(1));
    assert_eq!(take(&mut reader), item(tail + 4));
}

/// A second drain of one reader, after the first turned to `half`, resumes at `tail` again.
#[test]
fn a_new_drain_of_a_reader_resumes_at_tail_again() {
    let heap = Heap::new(RING);
    let mut writer = writer_of(&heap);
    let (mut reader, _, tail) = lapped_drain(&heap, &mut writer);
    let half = turned(&heap, &mut writer, &mut reader, tail);
    reader.stop_at(None);
    assert_eq!(
        drain(&mut reader),
        (half..203).map(item).collect::<Vec<_>>()
    );
    publish(&mut writer, 200);
    reader.stop_at(Some(writer.position()));
    let tail = (heap.loaded().tail() / SLOT) as u32;
    assert_eq!(take(&mut reader), Got::Lagged(u64::from(tail) - 203));
    // Turned again, then a drain to a later end with no `None` between: at `tail` again.
    turned(&heap, &mut writer, &mut reader, tail);
    publish(&mut writer, 200);
    reader.stop_at(Some(writer.position()));
    let tail = (heap.loaded().tail() / SLOT) as u32;
    assert!(matches!(take(&mut reader), Got::Lagged(_)));
    assert_eq!(take(&mut reader), item(tail));
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
    /// Headers loaded twice, stores between: the new first word, the old index, as on 32-bit.
    Torn,
}

/// Stores a staged reader waits for: 3 a record (header, payload cut at chunk 1); 6+ overwrite.
const STAGES: [u32; 6] = [0, 6, 1, 7, 2, 8];

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

    /// Between a look and a copy of `len` bytes: at times the publisher writes a ring's worth.
    fn pace(&self, len: usize) {
        if self.3 == Rhythm::Scattered {
            // Mostly up to three stores; one time in four, enough to write the whole ring anew.
            let turn = self.scatter(16);
            self.wait(if turn < 12 { turn % 4 } else { turn });
        }
        if self.3 == Rhythm::Staged && len > 8 {
            let turn = self.2.replace(self.2.get() + 1) as usize;
            self.wait(STAGES[turn % STAGES.len()]);
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

    fn store_frontier(&self, position: u64) {
        self.0.store_frontier(position);
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

    fn frontier(&self) -> u64 {
        self.0.frontier()
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
        self.pace(dst.len());
        if self.3 == Rhythm::Torn && dst.len() == 8 {
            let mut first = [0; 8];
            self.0.load_ring(position, &mut first);
            self.wait(self.scatter(3));
            self.0.load_ring(position, dst);
            dst[4..].copy_from_slice(&first[4..]);
            return;
        }
        self.0.load_ring(position, dst);
    }

    fn load_ring_extend(&self, position: u64, len: usize, dst: &mut Vec<u8>) {
        if self.3 == Rhythm::Torn && len == 8 {
            let start = dst.len();
            dst.resize(start + len, 0);
            self.load_ring(position, &mut dst[start..]);
            return;
        }
        self.pace(len);
        self.0.load_ring_extend(position, len, dst);
    }

    fn fence(&self) {
        // Between a copy and the load of `frontier` that judges it: a publish may fall between.
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
    race_of(
        rhythm,
        records,
        len_of,
        body,
        Start::Latest { at: 0, index: 0 },
    )
}

/// Payloads of words that pass for headers of items with no payload, of indexes two to four back.
fn passes_for_headers(index: u32, len: usize) -> Vec<u8> {
    let word = |back: u32| {
        let header = Header {
            kind: 0,
            len: 0,
            index: index.wrapping_sub(back),
        };
        header.bytes()
    };
    (2..).flat_map(word).take(len).collect()
}

/// [`race`] of payloads `payload_of` makes, read from `start`: unanchored, losses count from then.
fn race_of(
    rhythm: Rhythm,
    records: u32,
    len_of: fn(u32) -> usize,
    payload_of: fn(u32, usize) -> Vec<u8>,
    start: Start,
) -> u32 {
    let heap = Heap::new(64);
    let pace = Pace {
        stores: AtomicU32::new(0),
        done: AtomicBool::new(false),
    };
    let done = &pace.done;
    let paced = |memory, seed| Paced(memory, &pace, Cell::new(seed), rhythm);
    let mut writer = Writer::new(Paced(heap.stored(), &pace, Cell::new(0), rhythm));
    let attach = |seed, start| Reader::attach(paced(heap.loaded(), seed), start).unwrap();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Set when the thread ends, by a panic too: the readers' pauses wait for it.
            let _finished = Finished(done);
            for index in 0..records {
                let payload = payload_of(index, len_of(index));
                writer.put(Kind::Item, &payload).unwrap();
            }
        });
        scope.spawn(|| {
            let mut newest = attach(2, Start::Latest { at: 0, index: 0 });
            let mut above = 0;
            while !done.load(Ordering::Acquire) {
                match latest(&mut newest) {
                    Got::Item(index, payload) => {
                        let published = payload_of(index, len_of(index));
                        assert_eq!(payload, published, "record {index}");
                        assert!(index >= above, "{index} after {above}");
                        above = index + 1;
                    }
                    Got::Empty => std::thread::yield_now(),
                    other => panic!("newest: {other:?}"),
                }
            }
        });
        let mut reader = attach(1, start);
        let anchored = matches!(start, Start::Latest { .. });
        let (mut next, mut read, mut first) = (0u32, 0u32, None);
        loop {
            let finished = done.load(Ordering::Acquire);
            match take(&mut reader) {
                Got::Item(index, payload) => {
                    if !anchored && first.is_none() {
                        (next, first) = (index, Some(index));
                    }
                    assert_eq!(payload, payload_of(index, len_of(index)), "record {index}");
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
        // Every record from the first counted on was either returned or reported as lost, once.
        let counted = records - first.unwrap_or(0);
        assert_eq!(next, records);
        assert_eq!(u64::from(read) + reader.lost(), u64::from(counted));
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
fn a_copy_that_caught_an_overwrite_is_judged_by_a_frontier_stored_before_it() {
    let records = if cfg!(miri) { 120 } else { 30_000 };
    // Each payload copy comes after a wait that leaves the record whole, or does not.
    let read = race(Rhythm::Staged, records, |_| BODY);
    // Some whole record returned, so both judgements ran; how many depends on the machine's load.
    assert!(read > 0, "{read} of {records}");
}

/// Zero or header-like payloads, torn headers: none moves the reader unjudged, from either start.
#[test]
fn records_whose_payloads_pass_for_headers_are_returned_whole_or_counted() {
    let records = if cfg!(miri) { 60 } else { 20_000 };
    let lens = |index: u32| [0, 5, 24][index as usize % 3];
    let zeros = |_, len| vec![0; len];
    let starts = [Start::Latest { at: 0, index: 0 }, Start::Oldest];
    for start in starts {
        for rhythm in [Rhythm::Scattered, Rhythm::Torn] {
            race_of(rhythm, records, lens, zeros, start);
            race_of(rhythm, records, lens, passes_for_headers, start);
        }
    }
}

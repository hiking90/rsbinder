// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The writer and the reader over the memory they run on: one memfd, mapped read-write by the
//! writer and read-only, a second and third time, by the readers.

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::*;
use crate::image::Got;
use crate::layout::{create_topic, map_topic, MAX_RING, MIN_RING};

/// How long the overwrite test runs: `RSBINDER_TOPIC_SOAK_SECS`, two seconds without it.
fn soak() -> Duration {
    let secs = std::env::var("RSBINDER_TOPIC_SOAK_SECS")
        .ok()
        .and_then(|secs| secs.parse().ok());
    Duration::from_secs(secs.unwrap_or(2))
}

/// The payload length of record `index`: every padding, and copies long enough to be caught.
fn len_of(index: u32) -> usize {
    [0, 5, 24, 200, 1000, 2040][index as usize % 6]
}

/// The index, inverted: eight of these bytes, taken for a header, claim a length no record has.
fn body(index: u32) -> Vec<u8> {
    let bytes = (!index).to_le_bytes().into_iter().cycle();
    bytes.take(len_of(index)).collect()
}

#[test]
fn records_received_while_the_publisher_overwrites_are_whole() {
    let (fd, stored) = create_topic(MIN_RING).unwrap();
    let mut writer = Writer::new(stored);
    let from_start = Start::Latest { at: 0, index: 0 };
    let attach = || Reader::attach(map_topic(fd.as_fd(), MAX_RING).unwrap(), from_start).unwrap();
    let (mut reader, mut newest) = (attach(), attach());
    let done = AtomicBool::new(false);
    let deadline = Instant::now() + soak();
    let (mut next, mut read, mut newest_read) = (0u32, 0u64, 0u64);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            // Checked every 256 records: the clock is not part of what is measured.
            while writer.next_index() % 256 != 0 || Instant::now() < deadline {
                let index = writer.next_index();
                writer.put(Kind::Item, &body(index)).unwrap();
            }
            done.store(true, Ordering::Release);
        });
        scope.spawn(|| {
            let mut above = 0;
            while !done.load(Ordering::Acquire) {
                match Got::of(newest.try_latest()) {
                    Got::Item(index, payload) => {
                        assert_eq!(payload, body(index), "newest: record {index}");
                        assert!(index >= above, "newest: {index} after {above}");
                        (above, newest_read) = (index + 1, newest_read + 1);
                    }
                    Got::Empty => std::thread::yield_now(),
                    other => panic!("newest: {other:?}"),
                }
            }
        });
        loop {
            let finished = done.load(Ordering::Acquire);
            match Got::of(reader.try_recv()) {
                Got::Item(index, payload) => {
                    assert_eq!(payload, body(index), "record {index}");
                    assert_eq!(index, next, "an index out of order");
                    (next, read) = (next + 1, read + 1);
                    // A reader that falls behind now and then, so that it is lapped.
                    if read % 4096 == 0 {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
                Got::Lagged(lost) => next += lost as u32,
                Got::Empty if finished => break,
                Got::Empty => std::thread::yield_now(),
                other => panic!("{other:?}"),
            }
        }
    });
    let published = writer.next_index();
    println!(
        "published {published}, read {read}, lost {}, newest read {newest_read}",
        reader.lost()
    );
    // Every record was either returned or reported as lost, and none twice.
    assert_eq!(next, published);
    assert_eq!(read + reader.lost(), u64::from(published));
    assert!(read > 0 && reader.lost() > 0 && newest_read > 0);
    assert_eq!(reader.position(), writer.position());
}

#[test]
fn a_reader_attached_late_reads_what_the_ring_still_holds() {
    let (fd, stored) = create_topic(MIN_RING).unwrap();
    let mut writer = Writer::new(stored);
    for index in 0..1000 {
        writer.put(Kind::Item, &body(index)).unwrap();
    }
    writer.put(Kind::End, b"done").unwrap();
    let view = map_topic(fd.as_fd(), MAX_RING).unwrap();
    let mut reader = Reader::attach(view, Start::Oldest).unwrap();
    let mut indices = Vec::new();
    loop {
        match Got::of(reader.try_recv()) {
            Got::Item(index, payload) => {
                assert_eq!(payload, body(index));
                indices.push(index);
            }
            Got::End(payload) => {
                assert_eq!(payload, b"done");
                break;
            }
            other => panic!("{other:?}"),
        }
    }
    // The records the ring kept are the newest ones, without a gap.
    assert!(!indices.is_empty());
    let first = indices[0];
    assert_eq!(indices, (first..1000).collect::<Vec<_>>());
    assert_eq!(reader.lost(), 0);
}

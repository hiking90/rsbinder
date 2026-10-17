// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Values through the ring: encoded by [`TypedWriter`], decoded by [`TypedReader`], over heap
//! memory and, in `mapped`, over two mappings of one memfd.

use rsbinder::{ExceptionCode, Status, StatusCode};

use crate::layout::Heap;
use crate::publish::TypedWriter;
use crate::receive::TypedReader;
use crate::ring::{Kind, Loads, Reader, Start, Stores, Writer};
use crate::{Bytes, Closed, Encoding, Payload, PublishError, Record, RecvError};

/// A ring whose records carry at most 2040 bytes.
const RING: usize = 4096;

#[derive(rsbinder::Parcelable, Clone, Debug, Default, PartialEq)]
#[parcelable(descriptor = "rsbinder.topic.test.Sample")]
pub(crate) struct Sample {
    seq: i32,
    name: String,
    values: Vec<i64>,
}

#[derive(rsbinder::Parcelable, Debug, Default)]
struct WithBinder {
    binder: Option<rsbinder::SIBinder>,
}

#[derive(rsbinder::Parcelable, Debug, Default)]
struct WithFd {
    fd: Option<rsbinder::ParcelFileDescriptor>,
}

/// UTF-8 text, named "text".
#[derive(Debug, PartialEq)]
struct Text(String);

impl Payload for Text {
    fn type_name() -> &'static str {
        "text"
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> rsbinder::Result<()> {
        out.borrow(self.0.as_bytes());
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> rsbinder::Result<Self> {
        let text = std::str::from_utf8(record.bytes()).map_err(|_| StatusCode::BadValue)?;
        Ok(Self(text.to_owned()))
    }
}

/// Any bytes, also named "text": a second codec under the same name.
struct Raw(&'static [u8]);

impl Payload for Raw {
    fn type_name() -> &'static str {
        "text"
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> rsbinder::Result<()> {
        out.extend(self.0);
        Ok(())
    }

    fn decode(_: &mut Record<'_>) -> rsbinder::Result<Self> {
        unreachable!("only published")
    }
}

/// A byte, or `None`, which fails to encode.
#[derive(Debug, PartialEq)]
struct Picky(Option<u8>);

impl Payload for Picky {
    fn type_name() -> &'static str {
        "picky"
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> rsbinder::Result<()> {
        out.extend(&[self.0.ok_or(StatusCode::BadValue)?]);
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> rsbinder::Result<Self> {
        match record.bytes() {
            [byte] => Ok(Self(Some(*byte))),
            _ => Err(StatusCode::BadValue),
        }
    }
}

/// A binder that answers nothing: something to put in a parcelable.
struct Inert;

impl rsbinder::Remotable for Inert {
    fn descriptor() -> &'static str {
        "rsbinder.topic.test.Inert"
    }

    fn on_transact(
        &self,
        _: rsbinder::TransactionCode,
        _: &mut rsbinder::Parcel,
        _: &mut rsbinder::Parcel,
    ) -> rsbinder::Result<()> {
        Err(StatusCode::UnknownTransaction)
    }

    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> rsbinder::Result<()> {
        Ok(())
    }
}

fn publisher<T: Payload>(heap: &Heap) -> TypedWriter<T, impl Stores> {
    TypedWriter::new(Writer::new(heap.stored()))
}

fn subscriber<T: Payload>(heap: &Heap, start: Start) -> TypedReader<T, impl Loads> {
    TypedReader::new(Reader::attach(heap.loaded(), start).unwrap())
}

fn sample(seq: i32) -> Sample {
    Sample {
        seq,
        name: format!("sample {seq}"),
        values: (0..i64::from(seq)).collect(),
    }
}

#[test]
fn a_parcelable_is_named_by_its_descriptor_and_read_back_whole() {
    assert_eq!(Sample::type_name(), "rsbinder.topic.test.Sample");
    assert_eq!(Bytes::type_name(), "bytes");
    let heap = Heap::new(RING);
    let (topic, mut sub) = (publisher(&heap), subscriber(&heap, Start::Oldest));
    for seq in 0..20 {
        topic.publish(&sample(seq)).unwrap();
        assert_eq!(sub.try_recv(), Ok(sample(seq)));
    }
    assert_eq!(sub.try_recv(), Err(RecvError::Empty));
}

/// A record of every kind of piece, in lengths that share words: copied, borrowed, a parcelable.
#[derive(Debug, PartialEq)]
struct Mixed(Vec<u8>);

impl Mixed {
    fn bytes(&self) -> Vec<u8> {
        let mut bytes = b"ab".to_vec();
        bytes.extend_from_slice(&self.0);
        bytes.extend_from_slice(b"xyz");
        bytes.extend_from_slice(&rsbinder::to_bytes(&-7i32).unwrap());
        bytes.extend_from_slice(&self.0);
        bytes
    }
}

impl Payload for Mixed {
    fn type_name() -> &'static str {
        "mixed"
    }

    fn encode<'a>(&'a self, out: &mut Encoding<'a>) -> rsbinder::Result<()> {
        out.extend(b"ab");
        out.borrow(&self.0);
        out.extend(b"x");
        out.extend(b"yz");
        out.parcelable(&-7i32)?;
        out.borrow(&[]);
        out.borrow(&self.0);
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> rsbinder::Result<Self> {
        Ok(Self(record.take()))
    }
}

#[test]
fn a_record_is_its_pieces_in_order_whatever_words_they_share() {
    let heap = Heap::new(RING);
    let (topic, mut sub) = (publisher(&heap), subscriber::<Bytes>(&heap, Start::Oldest));
    // Borrowed runs of 0 to 17 bytes put every piece at every offset in a word; the ring wraps.
    for round in 0..8 {
        let batch: Vec<Mixed> = (0..18u8)
            .map(|n| Mixed(vec![n ^ round; n.into()]))
            .collect();
        assert_eq!(topic.publish_all(&batch), Ok(batch.len()));
        for value in &batch {
            assert_eq!(
                sub.try_recv(),
                Ok(Bytes(value.bytes())),
                "{} bytes",
                value.0.len()
            );
        }
    }
    assert_eq!(sub.try_recv(), Err(RecvError::Empty));
    assert!(heap.loaded().write() > RING as u64, "the ring did not wrap");
}

#[test]
fn a_record_the_subscriber_cannot_decode_is_skipped_and_the_next_one_read() {
    let heap = Heap::new(RING);
    let (topic, mut sub) = (
        publisher::<Raw>(&heap),
        subscriber::<Text>(&heap, Start::Oldest),
    );
    topic.publish(&Raw(b"\xff")).unwrap();
    topic.publish(&Raw(b"ok")).unwrap();
    assert_eq!(sub.try_recv(), Err(RecvError::Decode(StatusCode::BadValue)));
    assert_eq!(sub.try_recv(), Ok(Text("ok".into())));
    assert_eq!(sub.try_recv(), Err(RecvError::Empty));
}

#[test]
fn a_value_longer_than_a_record_carries_is_refused_before_anything_is_written() {
    let heap = Heap::new(RING);
    let (topic, mut sub) = (publisher(&heap), subscriber(&heap, Start::Oldest));
    let too_large = PublishError::TooLarge {
        bytes: 2041,
        max: 2040,
    };
    assert_eq!(topic.publish(&Bytes(vec![1; 2041])), Err(too_large.clone()));
    let batch = [Bytes(vec![1]), Bytes(vec![2; 2041]), Bytes(vec![3])];
    assert_eq!(topic.publish_all(&batch), Err(too_large));
    assert_eq!(sub.try_recv(), Err(RecvError::Empty));
    topic.publish(&Bytes(vec![4; 2040])).unwrap();
    assert_eq!(sub.try_recv(), Ok(Bytes(vec![4; 2040])));
}

#[test]
fn a_batch_with_a_value_that_fails_to_encode_writes_nothing() {
    let heap = Heap::new(RING);
    let (topic, mut sub) = (publisher(&heap), subscriber(&heap, Start::Oldest));
    let batch = [Picky(Some(1)), Picky(None), Picky(Some(3))];
    assert_eq!(
        topic.publish_all(&batch),
        Err(PublishError::Encode(StatusCode::BadValue))
    );
    let memory = heap.loaded();
    assert_eq!((memory.write(), memory.seq().word().load()), (0, 0));
    assert_eq!(sub.try_recv(), Err(RecvError::Empty));
    // An empty batch writes nothing either, and wakes no one.
    assert_eq!(topic.publish_all(&[]), Ok(0));
    assert_eq!(memory.seq().word().load(), 0);
    let batch = [Picky(Some(1)), Picky(Some(2)), Picky(Some(3))];
    assert_eq!(topic.publish_all(&batch), Ok(3));
    // One announcement for the batch.
    assert_eq!(memory.seq().word().load(), 1);
    for value in batch {
        assert_eq!(sub.try_recv(), Ok(value));
    }
}

#[test]
fn a_binder_or_a_file_descriptor_in_the_value_is_refused() {
    let heap = Heap::new(RING);
    let topic = publisher::<WithBinder>(&heap);
    let binder = rsbinder::Interface::as_binder(&rsbinder::Binder::new(Inert));
    let value = WithBinder {
        binder: Some(binder),
    };
    assert_eq!(
        topic.publish(&value),
        Err(PublishError::Encode(StatusCode::BadType))
    );
    let heap = Heap::new(RING);
    let topic = publisher::<WithFd>(&heap);
    let file = std::fs::File::open("/dev/null").unwrap();
    let value = WithFd {
        fd: Some(file.into()),
    };
    assert_eq!(
        topic.publish(&value),
        Err(PublishError::Encode(StatusCode::FdsNotAllowed))
    );
    assert_eq!(heap.loaded().write(), 0);
    // Without the object the same type is carried.
    topic.publish(&WithFd::default()).unwrap();
    let mut sub = subscriber::<WithFd>(&heap, Start::Oldest);
    assert!(sub.try_recv().unwrap().fd.is_none());
}

#[test]
fn losses_and_the_newest_value_come_through_as_the_ring_reports_them() {
    let heap = Heap::new(RING);
    let topic = publisher(&heap);
    let mut sub = subscriber(&heap, Start::Latest { at: 0, index: 0 });
    // 300 records of 32 bytes in a ring of 128; the reader resumes at the newer half, 64 back.
    for index in 0..300u32 {
        topic.publish(&Bytes(vec![index as u8; 24])).unwrap();
    }
    assert_eq!(sub.try_recv(), Err(RecvError::Lagged(236)));
    assert_eq!(sub.try_recv(), Ok(Bytes(vec![236; 24])));
    assert_eq!(sub.try_latest(), Ok(Bytes(vec![299u32 as u8; 24])));
    assert_eq!(sub.try_latest(), Err(RecvError::Empty));
}

#[test]
fn close_ends_the_topic_with_its_status_after_the_last_value() {
    let heap = Heap::new(RING);
    let (topic, mut sub) = (publisher(&heap), subscriber(&heap, Start::Oldest));
    topic.publish(&Bytes(b"last".to_vec())).unwrap();
    topic.close(&Status::from(ExceptionCode::None));
    assert_eq!(topic.publish(&Bytes(vec![])), Err(PublishError::Closed));
    assert_eq!(
        topic.publish_all(&[Bytes(vec![])]),
        Err(PublishError::Closed)
    );
    // A second close writes nothing.
    topic.close(&Status::from(ExceptionCode::IllegalState));
    assert_eq!(sub.try_recv(), Ok(Bytes(b"last".to_vec())));
    let ended = Err(RecvError::Closed(Closed::End));
    assert_eq!(sub.try_recv(), ended);
    assert_eq!(sub.try_recv(), ended);
    assert_eq!(sub.try_latest(), ended);

    let heap = Heap::new(RING);
    let (topic, mut sub) = (
        publisher::<Bytes>(&heap),
        subscriber::<Bytes>(&heap, Start::Oldest),
    );
    let status = Status::new_service_specific_error(3, Some("gone".into()));
    topic.close(&status);
    let Err(RecvError::Closed(Closed::Status(read))) = sub.try_recv() else {
        panic!("the end record carries the status");
    };
    assert_eq!(read, status);
    assert_eq!(read.message(), Some("gone"));
    let mut late = subscriber::<Bytes>(&heap, Start::Last);
    assert_eq!(
        late.try_latest(),
        Err(RecvError::Closed(Closed::Status(status)))
    );
}

#[test]
fn an_end_record_too_short_for_a_status_is_corrupted() {
    let heap = Heap::new(RING);
    let mut writer = Writer::new(heap.stored());
    writer.put(Kind::End, &[0; 7]).unwrap();
    let mut sub = subscriber::<Bytes>(&heap, Start::Oldest);
    let corrupted = Err(RecvError::Closed(Closed::Corrupted));
    assert_eq!(sub.try_recv(), corrupted);
    assert_eq!(sub.try_recv(), corrupted);
}

#[cfg(all(not(miri), any(target_os = "linux", target_os = "android")))]
mod mapped {
    use std::os::fd::AsFd;

    use rsbinder::fmq::Error;

    use super::*;
    use crate::layout::{map_topic, MAX_RING};
    use crate::PublisherPolicy;

    fn subscriber<T: Payload>(fd: std::os::fd::BorrowedFd<'_>) -> TypedReader<T, impl Loads> {
        let memory = map_topic(fd, MAX_RING).unwrap();
        TypedReader::new(Reader::attach(memory, Start::Oldest).unwrap())
    }

    #[test]
    fn parcelables_and_bytes_reach_a_second_mapping_unchanged() {
        let (fd, _, topic) = TypedWriter::create(&PublisherPolicy::default()).unwrap();
        let mut sub = subscriber::<Sample>(fd.as_fd());
        let samples: Vec<_> = (0..100).map(sample).collect();
        topic.publish(&samples[0]).unwrap();
        assert_eq!(topic.publish_all(&samples[1..]), Ok(99));
        for sample in samples {
            assert_eq!(sub.try_recv(), Ok(sample));
        }
        assert_eq!(sub.try_recv(), Err(RecvError::Empty));

        let (fd, _, topic) = TypedWriter::create(&PublisherPolicy::default()).unwrap();
        let mut sub = subscriber::<Bytes>(fd.as_fd());
        let values: Vec<_> = (0..100u8)
            .map(|byte| Bytes(vec![byte; byte.into()]))
            .collect();
        assert_eq!(topic.publish_all(&values), Ok(100));
        for value in values {
            assert_eq!(sub.try_recv(), Ok(value));
        }
    }

    #[test]
    fn a_ring_under_4_kib_is_refused_and_one_sized_for_a_payload_is_made() {
        let small = PublisherPolicy {
            ring_bytes: 4095,
            ..PublisherPolicy::default()
        };
        let made = TypedWriter::<Bytes, _>::create(&small);
        assert!(matches!(made.err(), Some(Error::BadValue(_))));
        let policy = PublisherPolicy::for_payload(1, 1);
        assert_eq!(policy.ring_bytes, 4096);
        let (fd, _, topic) = TypedWriter::<Bytes, _>::create(&policy).unwrap();
        topic.publish(&Bytes(vec![7; 2040])).unwrap();
        assert_eq!(subscriber(fd.as_fd()).try_recv(), Ok(Bytes(vec![7; 2040])));
    }
}

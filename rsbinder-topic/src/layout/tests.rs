// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The two memory objects as the kernel hands them out: made, sealed, sent as an fd and mapped a
//! second time by the side that only reads.

use super::*;
use crate::image::Got;
use crate::ring::{Kind, Reader, Start, Writer};

/// A populated, sealed memfd of `size` bytes holding the 4-byte `words` and zeros.
fn sealed(size: u64, words: &[(usize, u32)]) -> OwnedFd {
    let fd = shm::create(size, &CreateOptions::new("forged").populate(true)).unwrap();
    let mapping = Mapping::map(fd.as_fd(), 0, size, Access::ReadWrite).unwrap();
    for (at, value) in words {
        // SAFETY: an aligned word inside the page-aligned mapping made above; atomics only.
        let word = unsafe { mapping.as_ptr().add(*at).cast::<AtomicU32>().as_ref() };
        word.store(*value, Ordering::Relaxed);
    }
    seal(fd.as_fd()).unwrap();
    fd
}

fn bad_value<T>(result: Result<T>) -> &'static str {
    match result {
        Err(Error::BadValue(what)) => what,
        Err(other) => panic!("expected BadValue, got {other:?}"),
        Ok(_) => panic!("expected BadValue, got a value"),
    }
}

const TOPIC_SIZE: u64 = (topic::RING + MIN_RING) as u64;

#[test]
fn topic_memory_is_written_through_one_mapping_and_read_through_another() {
    let (fd, stored) = create_topic(MIN_RING).unwrap();
    assert_eq!(shm::region_size(fd.as_fd()), Ok(TOPIC_SIZE));
    let view = map_topic(fd.as_fd(), MIN_RING).unwrap();
    assert_eq!(Loads::capacity(&view), MIN_RING as u64);
    let seq = map_topic(fd.as_fd(), MAX_RING).unwrap();
    let mut writer = Writer::new(stored);
    let mut reader = Reader::attach(view, Start::Oldest).unwrap();
    assert_eq!(Got::of(reader.try_recv()), Got::Empty);
    writer.put(Kind::Item, b"through shared memory").unwrap();
    assert_eq!(
        Got::of(reader.try_recv()),
        Got::Item(0, b"through shared memory".to_vec())
    );
    // The word a subscriber sleeps on moves once for each announcement.
    assert_eq!(seq.seq().word().load(), 0);
    writer.announce();
    writer.announce();
    assert_eq!(seq.seq().word().load(), 2);
}

#[test]
fn a_ring_is_rounded_up_to_eight_bytes_and_bounded() {
    assert!(bad_value(create_topic(MIN_RING - 1).map(|_| ())).contains("ring size"));
    assert!(bad_value(create_topic(0).map(|_| ())).contains("ring size"));
    // Refused before anything is allocated.
    assert!(bad_value(create_topic(MAX_RING + 1).map(|_| ())).contains("ring size"));
    let (fd, stored) = create_topic(MIN_RING + 1).unwrap();
    assert_eq!(Stores::capacity(&stored), MIN_RING as u64 + 8);
    assert_eq!(shm::region_size(fd.as_fd()), Ok(TOPIC_SIZE + 8));
}

#[test]
fn a_subscriber_can_neither_write_nor_resize_topic_memory() {
    let (fd, _stored) = create_topic(MIN_RING).unwrap();
    let sealed = shm::seals(fd.as_fd()).unwrap();
    assert!(sealed.contains(Seals::GROW | Seals::SHRINK | Seals::FUTURE_WRITE | Seals::SEAL));
    let writable = Mapping::map(fd.as_fd(), 0, TOPIC_SIZE, Access::ReadWrite);
    assert_eq!(writable.err(), Some(Error::Os(rustix::io::Errno::PERM)));
    // What `shm::verify_received` asks of a peer's memfd is what this crate makes.
    assert_eq!(shm::verify_received(fd.as_fd()), Ok(TOPIC_SIZE));
}

#[test]
fn an_fd_that_is_not_topic_memory_is_not_mapped() {
    let map = |fd: &OwnedFd| map_topic(fd.as_fd(), MAX_RING).map(|_| ());
    let null: OwnedFd = std::fs::File::open("/dev/null").unwrap().into();
    assert!(bad_value(map(&null)).contains("not a sealable file"));
    let options = CreateOptions::new("unsealed").populate(true);
    let unsealed = shm::create(TOPIC_SIZE, &options).unwrap();
    assert!(bad_value(map(&unsealed)).contains("not sealed"));

    // Sealed and populated as topic memory is, without the words that say it is.
    let blank = sealed(TOPIC_SIZE, &[]);
    assert_eq!(bad_value(map(&blank)), "received memfd is not topic memory");
    let marked = [(topic::MAGIC, TOPIC_MAGIC), (topic::VERSION, VERSION + 1)];
    let other_version = sealed(TOPIC_SIZE, &marked);
    assert_eq!(
        bad_value(map(&other_version)),
        "topic memory of another layout version"
    );
    let marked = [(topic::MAGIC, TOPIC_MAGIC), (topic::VERSION, VERSION)];
    assert!(map(&sealed(TOPIC_SIZE, &marked)).is_ok());

    // A size no publisher makes: below the smallest ring, off the grid, above the caller's bound.
    for size in [TOPIC_SIZE - 8, TOPIC_SIZE + 4, topic::RING as u64, 8] {
        let fd = sealed(size, &marked[..usize::from(size >= 0x18)]);
        assert!(bad_value(map(&fd)).contains("size outside"), "{size}");
    }
    let (fd, _stored) = create_topic(2 * MIN_RING).unwrap();
    assert!(bad_value(map_topic(fd.as_fd(), MIN_RING).map(|_| ())).contains("size outside"));
    assert!(map_topic(fd.as_fd(), 2 * MIN_RING).is_ok());
}

#[test]
fn a_control_page_is_written_by_the_subscriber_and_read_by_the_publisher() {
    let (fd, page) = ControlPage::create(0).unwrap();
    assert_eq!(shm::region_size(fd.as_fd()), Ok(0x80));
    assert_eq!(shm::verify_received(fd.as_fd()), Ok(0x80));
    let view = ControlView::map(fd.as_fd(), 0).unwrap();
    assert_eq!((view.state().load(), view.read(), view.lost()), (0, 0, 0));

    page.state().fetch_or(WAITING, Ordering::SeqCst);
    page.state().fetch_add(4, Ordering::SeqCst);
    assert_eq!(view.state().load_acquire(), 4 | WAITING);
    // A bit at a time: closing leaves the `seen` field and the waiting bit as they are.
    page.state().fetch_or(CLOSED, Ordering::SeqCst);
    page.state().fetch_and(!WAITING, Ordering::SeqCst);
    assert_eq!(view.state().load_acquire(), 4 | CLOSED);
    page.read().store(1 << 40, Ordering::Release);
    page.lost().store(7, Ordering::Release);
    assert_eq!((view.read(), view.lost()), (1 << 40, 7));

    // The publisher's side has no way to a writable mapping.
    let writable = Mapping::map(fd.as_fd(), 0, 0x80, Access::ReadWrite);
    assert_eq!(writable.err(), Some(Error::Os(rustix::io::Errno::PERM)));
}

#[test]
fn a_control_page_has_its_pin_words_after_the_header() {
    for (pins, size) in [(0, 0x80), (1, 0x88), (2, 0x88), (3, 0x90), (64, 0x180)] {
        let (fd, _page) = ControlPage::create(pins).unwrap();
        assert_eq!(shm::region_size(fd.as_fd()), Ok(size), "{pins} pins");
        assert!(ControlView::map(fd.as_fd(), pins).is_ok(), "{pins} pins");
    }
}

#[test]
fn an_fd_that_is_not_the_control_page_asked_for_is_not_mapped() {
    let map = |fd: BorrowedFd<'_>, pins| bad_value(ControlView::map(fd, pins).map(|_| ()));
    let (fd, _page) = ControlPage::create(1).unwrap();
    assert_eq!(map(fd.as_fd(), 0), "control page of another size");
    assert_eq!(map(fd.as_fd(), 3), "control page of another size");
    // One and two pin words take the same eight bytes: the size passes and the word decides.
    assert_eq!(map(fd.as_fd(), 2), "control page with another pin count");

    let blank = sealed(0x80, &[]);
    assert_eq!(
        map(blank.as_fd(), 0),
        "received memfd is not a control page"
    );
    let marked = [(control::MAGIC, CONTROL_MAGIC), (control::VERSION, 0)];
    let other_version = sealed(0x80, &marked);
    assert_eq!(
        map(other_version.as_fd(), 0),
        "control page of another layout version"
    );
    // Topic memory handed in where a control page belongs.
    let (topic, _stored) = create_topic(MIN_RING).unwrap();
    assert_eq!(map(topic.as_fd(), 0), "control page of another size");
    let options = CreateOptions::new("unsealed").populate(true);
    let unsealed = shm::create(0x80, &options).unwrap();
    assert!(map(unsealed.as_fd(), 0).contains("not sealed"));
}

#[test]
fn without_the_future_write_seal_nothing_is_made() {
    NO_FUTURE_WRITE.set(true);
    assert_eq!(create_topic(MIN_RING).err(), Some(Error::Unsupported));
    assert_eq!(ControlPage::create(0).err(), Some(Error::Unsupported));
    NO_FUTURE_WRITE.set(false);
    assert!(create_topic(MIN_RING).is_ok());
    assert!(ControlPage::create(0).is_ok());
}

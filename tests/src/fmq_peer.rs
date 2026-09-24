// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! rsbinder's half of the FMQ-over-binder harness (plan 12-fmq F2):
//! `fmqinterop.IFmqPeer` served, and driven, with `rsbinder::fmq`.
//!
//! The descriptor is what crosses binder; the ring itself is shared
//! memory. So a case has three legs: the descriptor's trip (`create` or
//! `adopt`), traffic from the service to the caller (`produce` on the
//! service, a local read here) and traffic back (`consume` on the
//! service, a local write here). Both legs of traffic use the blocking
//! operations, so each also shows the two sides' EventFlag words waking
//! each other.
//!
//! Every driver returns one `RESULT` line and whether it holds, so that
//! `src/bin/fmq_probe.rs` can print it on a device and
//! `tests/fmq_binder.rs` can assert it on a Linux host.

#![allow(non_snake_case)]

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Duration;

use rsbinder::fmq::{
    AttachPolicy, Descriptor, Error as FmqError, MQDescriptor, MessageQueue, SynchronizedReadWrite,
    NOT_EMPTY, NOT_FULL,
};
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/fmq_peer.rs"));

pub use fmqinterop::IFmqPeer::{BnFmqPeer, IFmqPeer};
pub use fmqinterop::QueueView::QueueView;

/// Longest any blocking operation waits for the other side.
pub const WAIT: Option<Duration> = Some(Duration::from_secs(10));
/// Items a traffic leg moves; well past the capacity, so the ring wraps
/// and both sides block many times.
pub const DEFAULT_COUNT: i32 = 5000;
pub const DEFAULT_CAPACITY: i32 = 64;

type Desc = MQDescriptor<i32, SynchronizedReadWrite>;
type FmqResult<T> = std::result::Result<T, FmqError>;

/// A receiver's demands: memfd with `F_SEAL_SHRINK` or an ashmem region,
/// and the EventFlag word the blocking legs need.
pub fn policy() -> AttachPolicy {
    AttachPolicy {
        max_capacity: 1 << 16,
        require_seal: true,
        require_event_flag: true,
    }
}

fn expected_sum(count: i32) -> i64 {
    (0..i64::from(count.max(0))).sum()
}

/// Write `0..count` in chunks of five, waiting for room.
pub fn write_all(q: &mut MessageQueue<i32>, count: i32) -> FmqResult<()> {
    let mut next = 0i32;
    let mut chunk = [0i32; 5];
    while next < count {
        let n = chunk.len().min((count - next) as usize);
        for slot in &mut chunk[..n] {
            *slot = next;
            next += 1;
        }
        q.write_blocking(&chunk[..n], NOT_FULL, NOT_EMPTY, WAIT)?;
    }
    Ok(())
}

/// Read `count` items in chunks of seven, waiting for them. Returns their
/// sum and whether they arrived as `0..count`.
pub fn read_all(q: &mut MessageQueue<i32>, count: i32) -> FmqResult<(i64, bool)> {
    let mut next = 0i32;
    let mut sum = 0i64;
    let mut ordered = true;
    let mut chunk = [0i32; 7];
    while next < count {
        let n = chunk.len().min((count - next) as usize);
        q.read_blocking(&mut chunk[..n], NOT_EMPTY, NOT_FULL, WAIT)?;
        for item in &chunk[..n] {
            if *item != next {
                ordered = false;
            }
            sum += i64::from(*item);
            next += 1;
        }
    }
    Ok((sum, ordered))
}

/// What the fd behind a descriptor is, as this process sees it.
pub fn memory_kind(desc: &Desc) -> String {
    let Some(fd) = desc.handle.fds.first() else {
        return "none".into();
    };
    let fd = fd.as_fd();
    if rsbinder::fmq::shm::is_ashmem_fd(fd) {
        "ashmem".into()
    } else if rsbinder::fmq::shm::shrink_sealed(fd) {
        "memfd:sealed".into()
    } else {
        "unsealed".into()
    }
}

// ---- the service ----------------------------------------------------------

fn library_error(e: FmqError) -> Status {
    Status::new_service_specific_error(i32::from(StatusCode::from(e)), Some(format!("{e:?}")))
}

fn no_queue() -> Status {
    Status::from(ExceptionCode::IllegalState)
}

/// The service: one queue at a time, made here or adopted from the caller.
#[derive(Default)]
pub struct Peer {
    queue: Mutex<Option<MessageQueue<i32>>>,
}

impl Interface for Peer {}

impl IFmqPeer for Peer {
    fn r#create(&self, capacity: i32) -> BinderResult<Desc> {
        let capacity = usize::try_from(capacity).map_err(|_| StatusCode::BadValue)?;
        let q = MessageQueue::<i32>::create(capacity, true).map_err(library_error)?;
        let desc = Desc::try_from(q.descriptor().map_err(library_error)?)?;
        *self.queue.lock().unwrap() = Some(q);
        Ok(desc)
    }

    fn r#adopt(&self, desc: &Desc) -> BinderResult<i32> {
        let kind = memory_kind(desc);
        let q = MessageQueue::<i32>::attach(&Descriptor::try_from(desc)?, &policy())
            .map_err(library_error)?;
        let capacity = i32::try_from(q.capacity()).map_err(|_| StatusCode::BadValue)?;
        eprintln!("fmq_peer: adopted a {capacity}-item queue ({kind})");
        *self.queue.lock().unwrap() = Some(q);
        Ok(capacity)
    }

    fn r#produce(&self, count: i32) -> BinderResult<i32> {
        let mut guard = self.queue.lock().unwrap();
        let q = guard.as_mut().ok_or_else(no_queue)?;
        write_all(q, count).map_err(library_error)?;
        Ok(count)
    }

    fn r#consume(&self, count: i32) -> BinderResult<i64> {
        let mut guard = self.queue.lock().unwrap();
        let q = guard.as_mut().ok_or_else(no_queue)?;
        let (sum, _) = read_all(q, count).map_err(library_error)?;
        Ok(sum)
    }

    fn r#view(&self, count: i32) -> BinderResult<QueueView> {
        let mut guard = self.queue.lock().unwrap();
        let q = guard.as_mut().ok_or_else(no_queue)?;
        let report = |r: FmqResult<usize>| r.map(|n| n as i64).unwrap_or(-1);
        let availableToRead = report(q.available_to_read());
        let availableToWrite = report(q.available_to_write());
        let mut items = vec![0i32; usize::try_from(count).map_err(|_| StatusCode::BadValue)?];
        let (readResult, sum) = match q.read(&mut items) {
            Ok(true) => (1, items.iter().map(|i| i64::from(*i)).sum()),
            Ok(false) => (0, 0),
            Err(_) => (-1, 0),
        };
        Ok(QueueView {
            availableToRead,
            availableToWrite,
            readResult,
            sum,
        })
    }
}

/// Publish a `Peer` as `name` on the kernel binder and serve it; returns
/// only when the loop ends.
pub fn serve(name: &str) -> Result<()> {
    let peer = BnFmqPeer::new_binder(Peer::default());
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&peer))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

// ---- the driver -----------------------------------------------------------

/// A proxy for the peer `name`, refused when the name resolves to a
/// binder of this process.
pub fn connect(name: &str) -> Result<Strong<dyn IFmqPeer>> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    if binder.as_remote().is_none() {
        return Err(StatusCode::BadType);
    }
    <dyn IFmqPeer as FromIBinder>::try_from(binder)
}

/// One `RESULT` line and whether every check behind it held.
#[derive(Debug)]
pub struct Outcome {
    pub line: String,
    pub ok: bool,
}

fn word(ok: bool) -> &'static str {
    if ok {
        "ok"
    } else {
        "FAIL"
    }
}

/// The two traffic legs over an attached queue: the peer produces while
/// we read, then we write while the peer consumes. Returns `(in, out)`.
fn traffic(peer: &Strong<dyn IFmqPeer>, q: &mut MessageQueue<i32>, count: i32) -> (bool, bool) {
    // The service's half of each leg blocks in its handler, so it runs
    // on a thread of ours while this thread does the local half.
    let remote = peer.clone();
    let producer = thread::spawn(move || remote.r#produce(count));
    let inbound = match read_all(q, count) {
        Ok((sum, ordered)) => ordered && sum == expected_sum(count),
        Err(e) => {
            eprintln!("fmq_peer: local read failed: {e:?}");
            false
        }
    };
    let inbound = inbound && matches!(producer.join(), Ok(Ok(n)) if n == count);

    let remote = peer.clone();
    let consumer = thread::spawn(move || remote.r#consume(count));
    let outbound = match write_all(q, count) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("fmq_peer: local write failed: {e:?}");
            false
        }
    };
    let outbound = outbound && matches!(consumer.join(), Ok(Ok(sum)) if sum == expected_sum(count));
    (inbound, outbound)
}

/// The peer makes the queue; its descriptor comes back as a return value.
pub fn server_queue(peer: &Strong<dyn IFmqPeer>, count: i32, capacity: i32) -> Outcome {
    let desc = match peer.r#create(capacity) {
        Ok(desc) => desc,
        Err(e) => {
            return Outcome {
                line: format!("RESULT server-queue {count} create=err:{e:?}"),
                ok: false,
            }
        }
    };
    let kind = memory_kind(&desc);
    let mut q = match Descriptor::try_from(&desc)
        .map_err(|e| format!("{e:?}"))
        .and_then(|d| MessageQueue::<i32>::attach(&d, &policy()).map_err(|e| format!("{e:?}")))
    {
        Ok(q) => q,
        Err(e) => {
            return Outcome {
                line: format!("RESULT server-queue {count} kind={kind} attach=err:{e:?}"),
                ok: false,
            }
        }
    };
    let capacity_ok = q.capacity() == capacity as usize;
    let (inbound, outbound) = traffic(peer, &mut q, count);
    Outcome {
        line: format!(
            "RESULT server-queue {count} kind={kind} capacity={} in={} out={}",
            q.capacity(),
            word(inbound),
            word(outbound)
        ),
        ok: capacity_ok && inbound && outbound,
    }
}

/// We make the queue; its descriptor goes out as an argument. Nothing is
/// written before `adopt` returns: a libfmq peer resets the counters when
/// it attaches.
pub fn client_queue(peer: &Strong<dyn IFmqPeer>, count: i32, capacity: i32) -> Outcome {
    let (mut q, desc) = match make_queue(capacity) {
        Ok(pair) => pair,
        Err(e) => {
            return Outcome {
                line: format!("RESULT client-queue {count} create=err:{e:?}"),
                ok: false,
            }
        }
    };
    let adopted = match peer.r#adopt(&desc) {
        Ok(n) => n,
        Err(e) => {
            return Outcome {
                line: format!("RESULT client-queue {count} adopt=err:{e:?}"),
                ok: false,
            }
        }
    };
    let (inbound, outbound) = traffic(peer, &mut q, count);
    Outcome {
        line: format!(
            "RESULT client-queue {count} adopted={adopted} in={} out={}",
            word(inbound),
            word(outbound)
        ),
        ok: adopted == capacity && inbound && outbound,
    }
}

fn make_queue(capacity: i32) -> Result<(MessageQueue<i32>, Desc)> {
    let q = MessageQueue::<i32>::create(capacity as usize, true)?;
    let desc = Desc::try_from(q.descriptor()?)?;
    Ok((q, desc))
}

/// The queue's two counters, reached through a mapping of our own, the
/// way a hostile peer would reach them.
struct Counters {
    base: *mut u8,
    len: usize,
}

impl Counters {
    fn map(desc: &Descriptor) -> Result<Self> {
        let len = rsbinder::fmq::shm::region_size(desc.fds[0].as_fd())? as usize;
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
        .map_err(|_| StatusCode::NoMemory)?
        .cast::<u8>();
        Ok(Self { base, len })
    }

    fn set(&self, read: u64, write: u64) {
        // SAFETY: offsets 0 and 8 of the layout `create` made, inside the
        // mapping.
        unsafe {
            (*self.base.cast::<AtomicU64>()).store(read, Ordering::SeqCst);
            (*self.base.add(8).cast::<AtomicU64>()).store(write, Ordering::SeqCst);
        }
    }
}

impl Drop for Counters {
    fn drop(&mut self) {
        // SAFETY: exactly what `mmap` returned.
        let _ = unsafe { rustix::mm::munmap(self.base.cast(), self.len) };
    }
}

/// S4: we make the queue, the peer adopts it, and we damage the counters
/// one way at a time, asking the peer what its library reports after
/// each. Nothing about the report is asserted beyond the call returning:
/// the point is to record how the other library answers a ring that
/// breaks its invariants. One line per damage.
pub fn corrupt(peer: &Strong<dyn IFmqPeer>, capacity: i32) -> Vec<Outcome> {
    let fail = |what: &str, e: &dyn std::fmt::Debug| {
        vec![Outcome {
            line: format!("RESULT corrupt {what}=err:{e:?}"),
            ok: false,
        }]
    };
    let (q, desc) = match make_queue(capacity) {
        Ok(pair) => pair,
        Err(e) => return fail("create", &e),
    };
    if let Err(e) = peer.r#adopt(&desc) {
        return fail("adopt", &e);
    }
    let counters = match q
        .descriptor()
        .map_err(Into::into)
        .and_then(|d| Counters::map(&d))
    {
        Ok(c) => c,
        Err(e) => return fail("map", &e),
    };
    let bytes = capacity as u64 * 4;
    let damages: [(&str, u64, u64); 3] = [
        // The reader claims to be past the writer.
        ("read-ahead", 16, 0),
        // More bytes in flight than the ring holds.
        ("over-capacity", 0, bytes * 3),
        // A write counter that is not a whole number of elements.
        ("unaligned", 0, 3),
    ];
    damages
        .iter()
        .map(|(what, read, write)| {
            counters.set(*read, *write);
            match peer.r#view(1) {
                Ok(v) => Outcome {
                    line: format!(
                        "RESULT corrupt {what} avail={}/{} read={} sum={}",
                        v.availableToRead, v.availableToWrite, v.readResult, v.sum
                    ),
                    ok: true,
                },
                Err(e) => Outcome {
                    line: format!("RESULT corrupt {what} view=err:{e:?}"),
                    ok: false,
                },
            }
        })
        .collect()
}

/// Every case, in order, as the device script and the host test run them.
pub fn run_all(peer: &Strong<dyn IFmqPeer>, count: i32, capacity: i32) -> Vec<Outcome> {
    let mut out = vec![
        server_queue(peer, count, capacity),
        client_queue(peer, count, capacity),
    ];
    out.extend(corrupt(peer, capacity));
    out
}

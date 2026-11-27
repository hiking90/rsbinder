//! rsbinder's side: the producer process (`IBench`), a stream consumer, a oneway-callback
//! consumer, and B1's twoway client.

use std::collections::VecDeque;
use std::io::Write;
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rsbinder::stream::{
    Receiver, ReceiverPolicy, RecvTimeoutError, Sink, SinkPolicy, StreamEndpoint, TryRecvError,
};
use rsbinder::shared_memory::{
    Allocation, BpMemory, BpMemoryHeap, HeapCache, IMemory, MemoryDealer, SharedMemory,
};
use rsbinder::*;
use rsbinder_topic::{
    Bytes, Closed, EventTopic, Publisher, PublisherBuilder, RecvError, Sample, Subscriber, Trust,
    Wake,
};

use crate::common::*;

#[allow(clippy::all, non_snake_case, non_camel_case_types, dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/bench.rs"));
}
use generated::ipccompare::Config::Config;
use generated::ipccompare::IBench::{BnBench, IBench};
use generated::ipccompare::Limits::Limits;
use generated::ipccompare::IBenchSink::{BnBenchSink, IBenchSink};

/// The producer's ring ceiling, `SinkPolicy::max_ring_bytes`'s default.
pub const MAX_RING: usize = 4 * 1024 * 1024;

/// B1's payload: `Parcel::write_blob`, which moves past 16 KiB from inline to a memfd.
#[derive(Debug, Default, Clone)]
pub struct BlobBytes(pub Vec<u8>);

impl Serialize for BlobBytes {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write_blob(&self.0, false)
    }
}

impl Deserialize for BlobBytes {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        let blob = parcel.read_blob()?;
        if !REUSE.load(Ordering::Relaxed) {
            return Ok(BlobBytes(blob.to_vec()?));
        }
        let mut out = spare();
        blob.read_into(&mut out)?;
        Ok(BlobBytes(out))
    }
}

/// `--payload blob-reuse`: a `BlobBytes` dropped leaves its `Vec` to the thread's next one.
static REUSE: AtomicBool = AtomicBool::new(false);

thread_local! {
    static SPARE: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// A `Vec` this thread dropped earlier, or an empty one.
fn spare() -> Vec<u8> {
    SPARE.try_with(|kept| kept.borrow_mut().pop()).ok().flatten().unwrap_or_default()
}

impl Drop for BlobBytes {
    fn drop(&mut self) {
        if REUSE.load(Ordering::Relaxed) && self.0.capacity() > 0 {
            let bytes = std::mem::take(&mut self.0);
            let _ = SPARE.try_with(|kept| {
                let mut kept = kept.borrow_mut();
                // A call holds two at most: its argument and its reply.
                if kept.len() < 2 {
                    kept.push(bytes);
                }
            });
        }
    }
}

impl SerializeOption for BlobBytes {}
impl DeserializeOption for BlobBytes {}

/// A stream item, encoded as AIDL `byte[]`.
#[derive(Debug, Default, Clone)]
pub struct StreamBytes(pub Vec<u8>);

impl Serialize for StreamBytes {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.0)
    }
}

impl Deserialize for StreamBytes {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(StreamBytes(parcel.read()?))
    }
}

impl SerializeOption for StreamBytes {}
impl DeserializeOption for StreamBytes {}

/// A ring that holds about four items, within what the producer accepts.
pub fn ring_for(size: usize) -> usize {
    ((size + 64) * 4).next_power_of_two().clamp(64 * 1024, MAX_RING)
}

/// The topic's ring: `depth` records of `size`, iceoryx2's subscriber buffer for that size.
pub fn topic_ring_for(size: usize) -> usize {
    let depth = crate::ix::buffer_for(size);
    let record = size.max(STAMP_BYTES).div_ceil(8) * 8 + 8;
    ((depth.max(1) + 1) * record).max(4096)
}

/// `events` waiting `ms` (none below 0) for subscribers, at most `window` records ahead (0: none).
fn backpressure(
    events: PublisherBuilder<Bytes, EventTopic>,
    ms: i32,
    window: i32,
) -> PublisherBuilder<Bytes, EventTopic> {
    let Ok(ms) = u64::try_from(ms) else {
        return events;
    };
    let timeout = Duration::from_millis(ms);
    match NonZeroUsize::new(window.max(0) as usize) {
        Some(window) => events.lossless_window(timeout, window),
        None => events.lossless(timeout),
    }
}

/// `--backpressure MS` as the producer takes it: -1 without the flag.
fn backpressure_arg(a: &Args) -> i32 {
    if a.has("backpressure") { a.num("backpressure", 0) as i32 } else { -1 }
}

/// `--window N` as the producer takes it: 0 without the flag.
fn window_arg(a: &Args) -> i32 {
    a.num("window", 0) as i32
}

/// What a consumer notes besides its histogram, one word: values lost, and a subscription taken
/// out by the publisher's wait.
fn extra(lost: u64, evicted: bool) -> String {
    let mut words = Vec::new();
    if lost > 0 {
        words.push(format!("lost={lost}"));
    }
    if evicted {
        words.push("evicted".to_string());
    }
    words.join(",")
}

#[derive(Default)]
struct Producer {
    streams: Mutex<Vec<Sink<StreamBytes>>>,
    callbacks: Mutex<Vec<Strong<dyn IBenchSink>>>,
    topic: Mutex<Option<Publisher<Bytes>>>,
    /// B10's state topic, made at the first `stateTopic()`.
    state: Mutex<Option<Publisher<Bytes>>>,
    /// The topic has a pool, and start() lends a slot per item instead of publishing a copy.
    loans: AtomicBool,
    failures: Arc<AtomicI32>,
    regions: Mutex<Regions>,
}

/// B12's server side: the client's region, and the dealer the replies come from.
#[derive(Default)]
struct Regions {
    shared: Option<SharedMemory>,
    scratch: Vec<u8>,
    cache: Option<Arc<HeapCache>>,
    /// The client's heap, held: the cache keeps a heap mapped only while some frame of it lives.
    peer_heap: Option<Arc<BpMemoryHeap>>,
    dealer: Option<Arc<MemoryDealer>>,
    frames: VecDeque<Allocation>,
}

/// Frames a B12 side keeps before it returns the oldest to its dealer.
const FRAMES_KEPT: usize = 2;

/// A heap `FRAMES_KEPT` frames and the next one fit, whatever the best-fit order leaves.
fn dealer_for(size: usize) -> Result<Arc<MemoryDealer>> {
    MemoryDealer::new((FRAMES_KEPT + 2) * size + 4096, 0)
}

impl Regions {
    fn echo_shared(&mut self, len: usize) -> Result<()> {
        let shared = self.shared.as_ref().ok_or(StatusCode::InvalidOperation)?;
        self.scratch.resize(len, 0);
        shared.read_at(0, &mut self.scratch)?;
        shared.write_at(0, &self.scratch)
    }

    fn echo_memory(&mut self, memory: &SIBinder) -> Result<SIBinder> {
        let cache = self.cache.get_or_insert_with(HeapCache::new).clone();
        let frame = BpMemory::new_with_cache(memory.clone(), cache);
        self.peer_heap = Some(frame.resolve()?);
        let len = IMemory::size(&frame);
        self.scratch.resize(len, 0);
        frame.read_at(0, &mut self.scratch)?;
        let dealer = match &self.dealer {
            Some(dealer) => dealer.clone(),
            None => self.dealer.insert(dealer_for(len)?).clone(),
        };
        let reply = dealer.allocate(len)?;
        reply.write_at(0, &self.scratch)?;
        let binder = reply.export();
        self.frames.push_back(reply);
        if self.frames.len() > FRAMES_KEPT {
            self.frames.pop_front();
        }
        Ok(binder)
    }
}

impl Producer {
    /// The topic, made with `events` on the first call; `loans` when `events` has a pool.
    fn topic_with(
        &self,
        events: PublisherBuilder<Bytes, EventTopic>,
        loans: bool,
    ) -> BinderResult<SIBinder> {
        let mut topic = self.topic.lock().expect("lock");
        if topic.is_none() {
            let publisher = events.build().map_err(|e| {
                eprintln!("rsb-producer: topic: {e}");
                Status::from(StatusCode::NoMemory)
            })?;
            *topic = Some(publisher);
            self.loans.store(loans, Ordering::SeqCst);
        }
        Ok(topic.as_ref().expect("made above").as_binder())
    }
}

impl Interface for Producer {}

impl IBench for Producer {
    fn r#addStream(&self, endpoint: &StreamEndpoint<StreamBytes>) -> BinderResult<()> {
        let policy = SinkPolicy::default().with_max_ring_bytes(MAX_RING);
        let sink = Sink::open_with(endpoint, &policy)?;
        self.streams.lock().expect("lock").push(sink);
        Ok(())
    }

    fn r#addCallback(&self, sink: &Strong<dyn IBenchSink>) -> BinderResult<()> {
        self.callbacks.lock().expect("lock").push(sink.clone());
        Ok(())
    }

    fn r#topic(
        &self,
        ring_bytes: i32,
        backpressure_ms: i32,
        window: i32,
    ) -> BinderResult<SIBinder> {
        let events = Publisher::<Bytes>::events().ring_bytes(ring_bytes.max(0) as usize);
        self.topic_with(backpressure(events, backpressure_ms, window), false)
    }

    fn r#loanTopic(
        &self,
        ring_bytes: i32,
        slot_bytes: i32,
        depth: i32,
        subscribers: i32,
        backpressure_ms: i32,
        window: i32,
    ) -> BinderResult<SIBinder> {
        let subscribers = subscribers.max(1) as usize;
        let events = Publisher::<Bytes>::events()
            .ring_bytes(ring_bytes.max(0) as usize)
            .max_subscribers(subscribers)
            .max_per_uid(subscribers)
            .lend(slot_bytes.max(0) as usize)
            .lend_keep(depth.max(0) as usize);
        self.topic_with(backpressure(events, backpressure_ms, window), true)
    }

    fn r#start(&self, count: i32, size: i32, rate_hz: i32) -> BinderResult<()> {
        let mut sinks = std::mem::take(&mut *self.streams.lock().expect("lock"));
        let callbacks = std::mem::take(&mut *self.callbacks.lock().expect("lock"));
        let topic = self.topic.lock().expect("lock").take();
        let loans = self.loans.load(Ordering::SeqCst);
        let failures = self.failures.clone();
        failures.store(0, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut item = StreamBytes(vec![0u8; (size.max(0) as usize).max(STAMP_BYTES)]);
            let mut record = Bytes(item.0.clone());
            let period = if rate_hz > 0 { 1_000_000_000 / rate_hz as u64 } else { 0 };
            let start = now_ns() + 1_000_000;
            for seq in 0..count.max(0) as u64 {
                if period > 0 {
                    sleep_until(start + seq * period);
                }
                if let Some(topic) = &topic {
                    stamp(&mut record.0, seq);
                    // A loan is written with one copy of the item, as iceoryx2's producer writes.
                    let published = if loans {
                        topic.loan_slice(record.0.len()).and_then(|mut loan| {
                            loan.copy_from_slice(&record.0);
                            loan.publish()
                        })
                    } else {
                        topic.publish(&record)
                    };
                    if published.is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                    continue;
                }
                stamp(&mut item.0, seq);
                for sink in sinks.iter_mut() {
                    if sink.send(&item).is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
                for callback in &callbacks {
                    if callback.deliver(&item.0).is_err() {
                        failures.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            for sink in sinks {
                let _ = sink.end();
            }
            // Dropping the publisher writes the end record its subscribers stop at.
            drop(topic);
        });
        Ok(())
    }

    fn r#failures(&self) -> BinderResult<i32> {
        Ok(self.failures.load(Ordering::SeqCst))
    }

    fn r#echo(&self, data: &BlobBytes) -> BinderResult<BlobBytes> {
        if !REUSE.load(Ordering::Relaxed) {
            return Ok(data.clone());
        }
        let mut reply = spare();
        reply.clear();
        reply.extend_from_slice(&data.0);
        Ok(BlobBytes(reply))
    }

    fn r#echoBytes(&self, data: &[u8]) -> BinderResult<Vec<u8>> {
        Ok(data.to_vec())
    }

    fn r#describe(&self, config: &Config) -> BinderResult<Config> {
        Ok(Config {
            r#id: config.r#id,
            r#timestamp: config.r#timestamp,
            r#gain: config.r#gain,
            r#enabled: config.r#enabled,
            r#name: config.r#name.clone(),
            r#tags: config.r#tags.clone(),
            r#samples: config.r#samples.clone(),
            r#limits: Limits { ..config.r#limits },
        })
    }

    fn r#value(&self) -> BinderResult<i64> {
        Ok(VALUE as i64)
    }

    fn r#stateTopic(&self) -> BinderResult<SIBinder> {
        let mut state = self.state.lock().expect("lock");
        if state.is_none() {
            let initial = Bytes(VALUE.to_le_bytes().to_vec());
            *state = Some(Publisher::<Bytes>::state().build(&initial)?);
        }
        Ok(state.as_ref().expect("made above").as_binder())
    }

    fn r#touch(&self, fd: &ParcelFileDescriptor, len: i64) -> BinderResult<i64> {
        match touch(fd, len.max(0) as usize) {
            Ok(answer) => Ok(answer as i64),
            Err(e) => {
                eprintln!("rsb-producer: touch: {e}");
                Err(StatusCode::BadValue.into())
            }
        }
    }

    fn r#shareRegion(&self, fd: &ParcelFileDescriptor) -> BinderResult<()> {
        let fd = fd.as_fd().try_clone_to_owned().map_err(|_| StatusCode::NoMemory)?;
        self.regions.lock().expect("lock").shared = Some(SharedMemory::from_fd(fd)?);
        Ok(())
    }

    fn r#echoShared(&self, len: i64) -> BinderResult<()> {
        Ok(self.regions.lock().expect("lock").echo_shared(len.max(0) as usize)?)
    }

    fn r#echoMemory(&self, memory: &SIBinder) -> BinderResult<SIBinder> {
        self.regions.lock().expect("lock").echo_memory(memory).map_err(|e| {
            eprintln!("rsb-producer: echoMemory: {e:?}");
            e.into()
        })
    }
}

/// B10's value, the same behind every implementation.
pub const VALUE: u64 = 0x1122_3344_5566_7788;

/// B8's argument.
fn config() -> Config {
    Config {
        r#id: 7,
        r#timestamp: now_ns() as i64,
        r#gain: 1.5,
        r#enabled: true,
        r#name: CONFIG_NAME.to_string(),
        r#tags: CONFIG_TAGS.iter().map(|t| t.to_string()).collect(),
        r#samples: (0..CONFIG_SAMPLES as i32).collect(),
        r#limits: Limits { r#low: -100, r#high: 100, r#scale: 0.25 },
    }
}

/// Child role `rsb-call`: `--count` calls of `--op`: `struct` (B8), `lookup` (B9: the name at
/// the service manager, to a proxy), `value` and `state` (B10: a call, and the newest value of
/// a state topic), `fd` (B11: a memfd of `--size` bytes handed over, mapped by the server).
pub fn call_client(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let name = a.str("name", "");
    let bench = connect(&name)?;
    let record = match a.str("op", "").as_str() {
        "struct" => {
            let config = config();
            time_calls(a, |_| -> Result<()> {
                let reply = bench.r#describe(&config)?;
                if reply.r#samples.len() != CONFIG_SAMPLES {
                    return Err(StatusCode::BadValue);
                }
                Ok(())
            })?
        }
        "lookup" => time_calls(a, |_| connect(&name).map(drop))?,
        "value" => time_calls(a, |_| -> Result<()> {
            if bench.r#value()? as u64 != VALUE {
                return Err(StatusCode::BadValue);
            }
            Ok(())
        })?,
        "state" => {
            let topic = bench.r#stateTopic()?;
            let mut subscriber = Subscriber::<Bytes>::builder().subscribe(&topic)?;
            // A reader keeps the value it read last, and asks the topic for a newer one.
            let mut held = Vec::new();
            time_calls(a, |_| -> Result<()> {
                match subscriber.try_latest() {
                    Ok(value) => held = value.0,
                    Err(RecvError::Empty) => {}
                    Err(e) => {
                        eprintln!("rsb-call: state: {e}");
                        return Err(StatusCode::InvalidOperation);
                    }
                }
                if std::hint::black_box(&held)[..] != VALUE.to_le_bytes() {
                    return Err(StatusCode::BadValue);
                }
                Ok(())
            })?
        }
        "fd" => {
            let mut shared = SharedBytes::new(a.num("size", 4096) as usize).map_err(|e| {
                eprintln!("rsb-call: memfd: {e}");
                StatusCode::NoMemory
            })?;
            let fd = shared.fd.try_clone().map_err(|_| StatusCode::NoMemory)?;
            let fd = ParcelFileDescriptor::new(fd);
            let len = shared.len() as i64;
            time_calls(a, |seq| -> Result<()> {
                let expected = shared.stamp(seq);
                if bench.r#touch(&fd, len)? as u64 != expected {
                    return Err(StatusCode::BadValue);
                }
                Ok(())
            })?
        }
        "shm" => {
            let size = (a.num("size", 4096) as usize).max(STAMP_BYTES + 1);
            let region = SharedMemory::create(size)?;
            bench.r#shareRegion(&region.to_parcel_fd()?)?;
            let (mut request, mut reply) = (vec![0u8; size], vec![0u8; size]);
            time_calls(a, |seq| -> Result<()> {
                stamp(&mut request, seq);
                region.write_at(0, &request)?;
                bench.r#echoShared(size as i64)?;
                region.read_at(0, &mut reply)?;
                same_stamp(&request, &reply)
            })?
        }
        "dealer" => {
            let size = (a.num("size", 4096) as usize).max(STAMP_BYTES + 1);
            let dealer = dealer_for(size)?;
            let cache = HeapCache::new();
            // Held, as the server holds this side's: without it every frame maps the heap again.
            let mut peer_heap = None;
            let mut frames = VecDeque::new();
            let (mut request, mut reply) = (vec![0u8; size], vec![0u8; size]);
            time_calls(a, |seq| -> Result<()> {
                stamp(&mut request, seq);
                let frame = dealer.allocate(size)?;
                frame.write_at(0, &request)?;
                let back = bench.r#echoMemory(&frame.export())?;
                frames.push_back(frame);
                if frames.len() > FRAMES_KEPT {
                    frames.pop_front();
                }
                let back = BpMemory::new_with_cache(back, cache.clone());
                peer_heap = Some(back.resolve()?);
                back.read_at(0, &mut reply)?;
                same_stamp(&request, &reply)
            })?
        }
        other => {
            eprintln!("rsb-call: unknown --op {other}");
            return Err(StatusCode::BadValue);
        }
    };
    println!("{}", record.line());
    Ok(())
}

/// B12's check: the reply carries the request's stamp and its last byte.
fn same_stamp(request: &[u8], reply: &[u8]) -> Result<()> {
    let same = request[..STAMP_BYTES] == reply[..STAMP_BYTES] && request.last() == reply.last();
    same.then_some(()).ok_or(StatusCode::BadValue)
}

fn ready(word: &str) {
    println!("{word}");
    std::io::stdout().flush().ok();
}

fn connect(name: &str) -> Result<Strong<dyn IBench>> {
    rsbinder::connect(Uri::kernel().with_service(name))
}

/// Child role `rsb-producer`: serves `IBench` under `--name` until killed.
pub fn producer(a: &Args) -> Result<()> {
    pin_server(a);
    REUSE.store(a.str("payload", "blob") == "blob-reuse", Ordering::Relaxed);
    let binder = BnBench::new_binder(Producer::default());
    let _server = rsbinder::serve(Uri::kernel())?.add(&a.str("name", ""), binder.as_binder())?.spawn()?;
    ready("READY");
    ProcessState::join_thread_pool()
}

/// Child role `rsb-stream`: one stream consumer.
pub fn stream_consumer(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let poll = a.str("wait", "block") == "poll";
    let bench = connect(&a.str("name", ""))?;
    let policy = ReceiverPolicy::default().with_ring_bytes(a.num("ring", 64 * 1024) as usize);
    let (mut rx, endpoint) = Receiver::<StreamBytes>::with_policy(&bench.as_binder(), &policy)?;
    bench.r#addStream(&endpoint)?;
    ready("SUBSCRIBED");
    let mut record = Record::new(a.num("warmup", 0));
    let mut idle: Option<Instant> = None;
    while record.received < count {
        let item = if poll {
            match rx.try_recv() {
                Ok(item) => item,
                Err(TryRecvError::Ended) => break,
                Err(TryRecvError::Empty) => {
                    if idle.get_or_insert_with(Instant::now).elapsed() > SILENCE {
                        break;
                    }
                    continue;
                }
                Err(TryRecvError::Failed(e)) => {
                    eprintln!("rsb-stream: {e:?}");
                    break;
                }
            }
        } else {
            match rx.recv_timeout(SILENCE) {
                Ok(item) => item,
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Ended) => break,
                Err(RecvTimeoutError::Failed(e)) => {
                    eprintln!("rsb-stream: {e:?}");
                    break;
                }
            }
        };
        idle = None;
        record.take(&item.0);
    }
    println!("{}", record.line());
    Ok(())
}

/// Child role `rsb-topic`: one topic subscriber. `--wait block` sleeps on the futex, `fd` on
/// the wake socket, `poll` spins on `try_recv`. Records the ring overwrote count toward `count`
/// and are reported as `lost=` in the note: without `--backpressure` a publish never waits for a
/// subscriber. A subscription the publisher's wait took out ends the run and notes `evicted`.
pub fn topic_consumer(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let wait = a.str("wait", "block");
    let bench = connect(&a.str("name", ""))?;
    let topic =
        bench.r#topic(a.num("ring", 64 * 1024) as i32, backpressure_arg(a), window_arg(a))?;
    let mut subscriber = Subscriber::<Bytes>::builder()
        .max_ring_bytes(1 << 30)
        .wake(if wait == "fd" { Wake::Fd } else { Wake::Futex })
        .subscribe(&topic)?;
    ready("SUBSCRIBED");
    let mut record = Record::new(a.num("warmup", 0));
    let mut lost = 0u64;
    let mut evicted = false;
    let mut idle: Option<Instant> = None;
    while record.received + lost < count {
        let got = if wait == "poll" { subscriber.try_recv() } else { subscriber.recv_timeout(SILENCE) };
        match got {
            Ok(item) => {
                idle = None;
                record.take(&item.0);
            }
            Err(RecvError::Empty) => {
                if idle.get_or_insert_with(Instant::now).elapsed() > SILENCE {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => lost += n,
            Err(RecvError::Closed(Closed::Evicted)) => {
                evicted = true;
                break;
            }
            Err(RecvError::Closed(_) | RecvError::Timeout) => break,
            Err(e) => {
                eprintln!("rsb-topic: {e}");
                break;
            }
        }
    }
    record.extra = extra(lost, evicted);
    println!("{}", record.line());
    Ok(())
}

/// Child role `rsb-topic-loan`: one subscriber of the topic with a pool. It receives each value
/// where it lies (`recv_sample`) and reads the stamp in place, or with `--touch yes` copies the
/// value out first, as the iceoryx2 subscriber does with the same flag. With `--trust yes` it
/// vouches for the publisher first (`trust_publisher`), as an iceoryx2 subscriber does, and
/// copies with a plain memory copy.
pub fn topic_loan_consumer(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let wait = a.str("wait", "block");
    let touch = a.str("touch", "no") == "yes";
    let bench = connect(&a.str("name", ""))?;
    let num = |key: &str| a.num(key, 0) as i32;
    let topic = bench.r#loanTopic(
        num("ring"),
        num("slot"),
        num("depth"),
        num("n"),
        backpressure_arg(a),
        window_arg(a),
    )?;
    let subscriber = Subscriber::<Bytes>::builder()
        .max_ring_bytes(1 << 30)
        .max_pool_bytes(usize::MAX)
        .wake(if wait == "fd" { Wake::Fd } else { Wake::Futex })
        .subscribe(&topic)?;
    ready("SUBSCRIBED");
    let record = if a.str("trust", "no") == "yes" {
        // SAFETY: the producer is this harness's own rsbinder-topic `Publisher`.
        let subscriber = unsafe { subscriber.trust_publisher() };
        take_samples(subscriber, a, |record, sample| {
            if touch {
                record.take(&sample.to_owned());
            } else {
                record.take(sample);
            }
        })
    } else {
        take_samples(subscriber, a, |record, sample| {
            if touch {
                record.take(&sample.to_owned());
            } else {
                // SAFETY: the producer is this harness's own rsbinder-topic `Publisher`.
                record.take(unsafe { sample.as_slice() });
            }
        })
    };
    println!("{}", record.line());
    Ok(())
}

/// Receive `--count` samples, or until the topic falls silent, handing each to `take`.
fn take_samples<M: Trust>(
    mut subscriber: Subscriber<Bytes, M>,
    a: &Args,
    mut take: impl FnMut(&mut Record, &mut Sample<'_, M>),
) -> Record {
    let count = a.num("count", 0);
    let poll = a.str("wait", "block") == "poll";
    let mut record = Record::new(a.num("warmup", 0));
    let mut lost = 0u64;
    let mut evicted = false;
    let mut idle: Option<Instant> = None;
    while record.received + lost < count {
        let got = if poll {
            subscriber.try_recv_sample()
        } else {
            subscriber.recv_sample_timeout(SILENCE)
        };
        match got {
            Ok(mut sample) => {
                idle = None;
                take(&mut record, &mut sample);
            }
            Err(RecvError::Empty) => {
                if idle.get_or_insert_with(Instant::now).elapsed() > SILENCE {
                    break;
                }
            }
            Err(RecvError::Lagged(n)) => lost += n,
            Err(RecvError::Closed(Closed::Evicted)) => {
                evicted = true;
                break;
            }
            Err(RecvError::Closed(_) | RecvError::Timeout) => break,
            Err(e) => {
                eprintln!("rsb-topic-loan: {e}");
                break;
            }
        }
    }
    record.extra = extra(lost, evicted);
    record
}

struct CallbackSink {
    state: Arc<(Mutex<Record>, Condvar)>,
}

impl Interface for CallbackSink {}

impl IBenchSink for CallbackSink {
    fn r#deliver(&self, data: &[u8]) -> BinderResult<()> {
        let (record, arrived) = &*self.state;
        record.lock().expect("lock").take(data);
        arrived.notify_all();
        Ok(())
    }
}

/// Child role `rsb-oneway`: one oneway-callback consumer.
pub fn callback_consumer(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let bench = connect(&a.str("name", ""))?;
    ProcessState::start_thread_pool();
    let state = Arc::new((Mutex::new(Record::new(a.num("warmup", 0))), Condvar::new()));
    let sink = BnBenchSink::new_binder(CallbackSink { state: state.clone() });
    bench.r#addCallback(&sink)?;
    ready("SUBSCRIBED");
    let (record, arrived) = &*state;
    let mut guard = record.lock().expect("lock");
    let mut seen = guard.received;
    while guard.received < count {
        let (next, timeout) = arrived.wait_timeout(guard, SILENCE).expect("lock");
        guard = next;
        if timeout.timed_out() && guard.received == seen {
            break;
        }
        seen = guard.received;
    }
    println!("{}", guard.line());
    Ok(())
}

/// Child role `rsb-echo`: B1's client, round trips of `--size` bytes each way.
pub fn echo_client(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let warmup = a.num("warmup", 0);
    let bench = connect(&a.str("name", ""))?;
    let mut request = BlobBytes(vec![0u8; a.num("size", 8) as usize]);
    // `--payload bytes`: an AIDL `byte[]`, in the transaction whatever its size.
    let inline = a.str("payload", "blob") == "bytes";
    REUSE.store(a.str("payload", "blob") == "blob-reuse", Ordering::Relaxed);
    wait_for_start(a);
    let mut record = Record::new(0);
    for seq in 0..count {
        stamp(&mut request.0, seq);
        let start = now_ns();
        let reply = if inline {
            BlobBytes(bench.r#echoBytes(&request.0)?)
        } else {
            bench.r#echo(&request)?
        };
        let end = now_ns();
        if reply.0.len() != request.0.len() {
            eprintln!("rsb-echo: reply of {} bytes", reply.0.len());
            break;
        }
        record.received += 1;
        if seq >= warmup {
            if record.first_ns == 0 {
                record.first_ns = start;
            }
            record.hist.saturating_record((end - start).max(1));
            record.last_ns = end;
        }
    }
    println!("{}", record.line());
    Ok(())
}

/// Driver side: how many sends the producer saw fail.
pub fn failures(name: &str) -> Result<i32> {
    Ok(connect(name)?.r#failures()?)
}

/// Driver side: starts the producer's run.
pub fn start(name: &str, count: u64, size: usize, rate: u64) -> Result<()> {
    connect(name)?.r#start(count as i32, size as i32, rate as i32).map_err(|s| s.into())
}

/// Lets the driver know how long to wait for a consumer that is still receiving.
pub fn expected(count: u64, rate: u64) -> Duration {
    let paced = if rate > 0 { Duration::from_nanos(count * 1_000_000_000 / rate) } else { Duration::ZERO };
    paced + SILENCE + Duration::from_secs(60)
}

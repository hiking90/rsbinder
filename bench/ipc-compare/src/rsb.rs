//! rsbinder's side: the producer process (`IBench`), a stream consumer, a oneway-callback
//! consumer, and B1's twoway client.

use std::io::Write;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use rsbinder::stream::{Receiver, ReceiverPolicy, Sink, SinkPolicy, StreamEndpoint};
use rsbinder::*;
use rsbinder_topic::{Bytes, Publisher, PublisherPolicy, RecvError, Subscriber, SubscriberPolicy, Wake};

use crate::common::*;

#[allow(clippy::all, non_snake_case, non_camel_case_types, dead_code)]
mod generated {
    include!(concat!(env!("OUT_DIR"), "/bench.rs"));
}
use generated::ipccompare::IBench::{BnBench, IBench};
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
        Ok(BlobBytes(parcel.read_blob()?.to_vec()?))
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
    PublisherPolicy::for_payload(size.max(STAMP_BYTES), depth).ring_bytes
}

#[derive(Default)]
struct Producer {
    streams: Mutex<Vec<Sink<StreamBytes>>>,
    callbacks: Mutex<Vec<Strong<dyn IBenchSink>>>,
    topic: Mutex<Option<Publisher<Bytes>>>,
    failures: Arc<AtomicI32>,
}

impl Interface for Producer {}

impl IBench for Producer {
    fn r#addStream(&self, endpoint: &StreamEndpoint<StreamBytes>) -> BinderResult<()> {
        let policy = SinkPolicy { max_ring_bytes: MAX_RING, ..SinkPolicy::default() };
        let sink = Sink::open_with(endpoint, &policy)?;
        self.streams.lock().expect("lock").push(sink);
        Ok(())
    }

    fn r#addCallback(&self, sink: &Strong<dyn IBenchSink>) -> BinderResult<()> {
        self.callbacks.lock().expect("lock").push(sink.clone());
        Ok(())
    }

    fn r#topic(&self, ring_bytes: i32) -> BinderResult<SIBinder> {
        let mut topic = self.topic.lock().expect("lock");
        if topic.is_none() {
            let policy = PublisherPolicy {
                ring_bytes: ring_bytes.max(0) as usize,
                ..PublisherPolicy::default()
            };
            let publisher = Publisher::<Bytes>::new(&policy).map_err(|e| {
                eprintln!("rsb-producer: topic: {e}");
                Status::from(StatusCode::NoMemory)
            })?;
            *topic = Some(publisher);
        }
        Ok(topic.as_ref().expect("made above").as_binder())
    }

    fn r#start(&self, count: i32, size: i32, rate_hz: i32) -> BinderResult<()> {
        let mut sinks = std::mem::take(&mut *self.streams.lock().expect("lock"));
        let callbacks = std::mem::take(&mut *self.callbacks.lock().expect("lock"));
        let topic = self.topic.lock().expect("lock").take();
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
                    if topic.publish(&record).is_err() {
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
        Ok(data.clone())
    }
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
    pin(a.num("cpu", 1) as usize);
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
    let policy = ReceiverPolicy {
        ring_bytes: a.num("ring", 64 * 1024) as usize,
        ..ReceiverPolicy::default()
    };
    let (mut rx, endpoint) = Receiver::<StreamBytes>::with_policy(&bench.as_binder(), &policy)?;
    bench.r#addStream(&endpoint)?;
    ready("SUBSCRIBED");
    let mut record = Record::new(a.num("warmup", 0));
    let mut idle: Option<Instant> = None;
    while record.received < count {
        let item = if poll {
            match rx.try_recv() {
                Ok(Some(item)) => item,
                Ok(None) if rx.is_finished() => break,
                Ok(None) => {
                    if idle.get_or_insert_with(Instant::now).elapsed() > SILENCE {
                        break;
                    }
                    continue;
                }
                Err(e) => {
                    eprintln!("rsb-stream: {e:?}");
                    break;
                }
            }
        } else {
            match rx.recv_timeout(SILENCE) {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(e) => {
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
/// and are reported as `lost=` in the note: a publish never waits for a subscriber.
pub fn topic_consumer(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let wait = a.str("wait", "block");
    let bench = connect(&a.str("name", ""))?;
    let topic = bench.r#topic(a.num("ring", 64 * 1024) as i32)?;
    let policy = SubscriberPolicy {
        max_ring_bytes: 1 << 30,
        wake: if wait == "fd" { Wake::Fd } else { Wake::Futex },
        ..SubscriberPolicy::default()
    };
    let mut subscriber = Subscriber::<Bytes>::request(&topic, &policy)?;
    ready("SUBSCRIBED");
    let mut record = Record::new(a.num("warmup", 0));
    let mut lost = 0u64;
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
            Err(RecvError::Closed(_) | RecvError::Timeout) => break,
            Err(e) => {
                eprintln!("rsb-topic: {e}");
                break;
            }
        }
    }
    if lost > 0 {
        record.extra = format!("lost={lost}");
    }
    println!("{}", record.line());
    Ok(())
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
    let mut record = Record::new(0);
    for seq in 0..count {
        stamp(&mut request.0, seq);
        let start = now_ns();
        let reply = bench.r#echo(&request)?;
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

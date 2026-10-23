//! B5 and B6 (plan 15 §8.2): the time of one `Publisher::publish`, in one process. A subscriber of
//! a publisher of its own process needs no binder device, and the publisher's side is all these
//! measure.
//!
//! B5: `--wake futex|fd --n N --rate HZ`. N subscribers, each on a thread of its own, wait with the
//! given wake; at a rate low enough that they sleep again between publishes, each publish finds
//! them waiting. The note gives how many were waiting, on average, at the publishes it sampled.
//!
//! B6: `--codec alloc|parcel|bytes --side publish|receive --size B`. `publish` times a publish
//! with no subscriber; `receive` times a `try_recv` of a record already in the ring, one
//! subscriber in this process. The codecs: `alloc` is a parcelable of one `byte[]` whose `Payload`
//! encodes it with `rsbinder::to_bytes` into a new buffer and copies that, and decodes it with
//! `rsbinder::from_bytes` of a copy; `parcel` is the same parcelable through the blanket
//! `Payload` impl; `bytes` is `Bytes`, the payload uncoded.

use std::sync::Arc;
use std::thread;

use rsbinder::{Deserialize, Parcel, ParcelableMetadata, Result, Serialize};
use rsbinder_topic::{Bytes, Encoding, Payload, Publisher, PublisherPolicy, Record, RecvError};
use rsbinder_topic::{Subscriber, SubscriberPolicy, Wake};

use crate::common::*;

/// A parcelable of one `byte[]`, published through the blanket impl.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Blob(pub Vec<u8>);

impl ParcelableMetadata for Blob {
    fn descriptor() -> &'static str {
        "ipccompare.Blob"
    }
}

impl Serialize for Blob {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        parcel.write(&self.0)
    }
}

impl Deserialize for Blob {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Ok(Self(parcel.read()?))
    }
}

/// [`Blob`] with a `Payload` that codes into a new buffer each way, and copies that buffer.
#[derive(Clone, PartialEq)]
pub struct AllocBlob(pub Blob);

impl Payload for AllocBlob {
    fn type_name() -> &'static str {
        Blob::descriptor()
    }

    fn encode(&self, out: &mut Encoding<'_>) -> Result<()> {
        out.extend(&rsbinder::to_bytes(&self.0)?);
        Ok(())
    }

    fn decode(record: &mut Record<'_>) -> Result<Self> {
        rsbinder::from_bytes(record.bytes()).map(Self)
    }
}

fn row(bench: &str, imp: &str, wait: &str, n: usize, size: usize, rate: u64, count: u64,
       hist: hdrhistogram::Histogram<u64>, note: String) {
    let mean = hist.mean();
    Row {
        bench,
        imp,
        wait,
        n,
        size,
        rate,
        count,
        warmup: count / 10,
        msgs_per_s: if mean > 0.0 { 1e9 / mean } else { 0.0 },
        hist: Some(hist),
        mean_ns: None,
        failures: 0,
        note,
    }
    .print();
}

pub fn b5(a: &Args) {
    let wake = match a.str("wake", "futex").as_str() {
        "fd" => Wake::Fd,
        _ => Wake::Futex,
    };
    let n = a.num("n", 1) as usize;
    let rate = a.num("rate", 1000).max(1);
    let count = a.num("count", 5000);
    let warmup = count / 10;
    pin(a.num("producer-cpu", 1) as usize);
    let policy = PublisherPolicy {
        max_subscribers: n.max(1),
        max_per_uid: n.max(1),
        ..PublisherPolicy::for_payload(64, 64)
    };
    let publisher = Arc::new(Publisher::<Bytes>::new(&policy).expect("a publisher"));
    let subscribing = SubscriberPolicy {
        wake,
        ..SubscriberPolicy::default()
    };
    let receiving: Vec<_> = (0..n)
        .map(|_| {
            let mut subscriber =
                Subscriber::<Bytes>::request(&publisher.as_binder(), &subscribing).expect("a subscription");
            thread::spawn(move || loop {
                match subscriber.recv() {
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(_) => return,
                }
            })
        })
        .collect();
    let mut hist = histogram();
    let (mut sampled, mut waiting) = (0u64, 0u64);
    let item = Bytes(vec![0u8; 64]);
    let period = 1_000_000_000 / rate;
    let start = now_ns() + 10_000_000;
    for seq in 0..count {
        sleep_until(start + seq * period);
        if seq % 100 == 0 {
            sampled += 1;
            waiting += publisher.stats().subscribers.iter().filter(|s| s.waiting).count() as u64;
        }
        let before = now_ns();
        publisher.publish(&item).expect("a publish");
        let took = now_ns() - before;
        if seq >= warmup {
            hist.saturating_record(took.max(1));
        }
    }
    let wait = if wake == Wake::Fd { "fd" } else { "futex" };
    let note = format!("waiting at a publish {:.1} of {n}", waiting as f64 / sampled.max(1) as f64);
    // Dropping the publisher writes the end record, which ends every receive loop.
    drop(publisher);
    for receiving in receiving {
        receiving.join().expect("a receiving thread");
    }
    row("b5", "rsb-topic", wait, n, 64, rate, count, hist, note);
}

pub fn b6(a: &Args) {
    let codec = a.str("codec", "alloc");
    let size = a.num("size", 64) as usize;
    let count = a.num("count", (1u64 << 30) / (size as u64).max(64)).clamp(10_000, 200_000);
    pin(a.num("producer-cpu", 1) as usize);
    let receive = a.str("side", "publish") == "receive";
    let policy = PublisherPolicy::for_payload(size + 8, 4);
    let value = vec![7u8; size];
    let mut hist = histogram();
    let checked = match codec.as_str() {
        "alloc" => cost(AllocBlob(Blob(value.clone())), receive, &policy, count, &mut hist),
        "bytes" => cost(Bytes(value.clone()), receive, &policy, count, &mut hist),
        _ => cost(Blob(value.clone()), receive, &policy, count, &mut hist),
    };
    let (side, n) = if receive { ("receive", 1) } else { ("publish", 0) };
    let note = format!("{side}; received what was published: {checked}");
    row("b6", "rsb-topic", &codec, n, size, 0, count, hist, note);
}

/// Time `count` publishes of `item`, or `count` receives of it; whether the last received is it.
fn cost<T: Payload + PartialEq + 'static>(
    item: T,
    receive: bool,
    policy: &PublisherPolicy,
    count: u64,
    hist: &mut hdrhistogram::Histogram<u64>,
) -> bool {
    let publisher = Publisher::<T>::new(policy).expect("a publisher");
    let mut subscriber = receive.then(|| {
        Subscriber::<T>::request(&publisher.as_binder(), &SubscriberPolicy::default())
            .expect("a subscription")
    });
    let mut same = true;
    for seq in 0..count {
        let took = match subscriber.as_mut() {
            None => {
                let before = now_ns();
                publisher.publish(&item).expect("a publish");
                now_ns() - before
            }
            Some(subscriber) => {
                publisher.publish(&item).expect("a publish");
                let before = now_ns();
                let got = subscriber.try_recv().expect("a record");
                let took = now_ns() - before;
                same = got == item;
                took
            }
        };
        if seq >= count / 10 {
            hist.saturating_record(took.max(1));
        }
    }
    same
}

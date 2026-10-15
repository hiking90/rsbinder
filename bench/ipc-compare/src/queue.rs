//! B4: the queue alone, two pinned threads in one process playing ping-pong through two queues,
//! as iceoryx2's `benchmarks/queue` does. Reported as round trip / 2, its "latency".

use std::sync::Barrier;
use std::time::Instant;

use iceoryx2_bb_lock_free::spsc::queue::Queue;
use rsbinder::fmq::{AttachPolicy, MessageQueue};

use crate::common::pin;

/// Elements per queue: the largest capacity iceoryx2's queue benchmark runs.
pub const CAPACITY: usize = 128;

fn ping_pong<A, B>(cpus: (usize, usize), count: u64, a: A, b: B) -> f64
where
    A: FnOnce(u64) + Send,
    B: FnOnce(u64) + Send,
{
    let barrier = Barrier::new(3);
    let mut elapsed = 0.0;
    std::thread::scope(|s| {
        let left = s.spawn(|| {
            pin(cpus.0);
            barrier.wait();
            a(count);
        });
        let right = s.spawn(|| {
            pin(cpus.1);
            barrier.wait();
            b(count);
        });
        barrier.wait();
        let start = Instant::now();
        left.join().expect("left thread");
        right.join().expect("right thread");
        elapsed = start.elapsed().as_nanos() as f64;
    });
    elapsed / (2 * count) as f64
}

pub fn fmq(cpus: (usize, usize), count: u64) -> rsbinder::fmq::Result<f64> {
    let pair = || -> rsbinder::fmq::Result<(MessageQueue<u64>, MessageQueue<u64>)> {
        let writer = MessageQueue::<u64>::create(CAPACITY, false)?;
        let policy = AttachPolicy {
            max_capacity: CAPACITY,
            require_seal: false,
            require_event_flag: false,
        };
        let reader = MessageQueue::<u64>::attach(&writer.descriptor()?, &policy)?;
        Ok((writer, reader))
    };
    let (mut a2b_w, mut a2b_r) = pair()?;
    let (mut b2a_w, mut b2a_r) = pair()?;
    Ok(ping_pong(
        cpus,
        count,
        move |count| {
            let mut one = [0u64];
            for i in 0..count {
                while !a2b_w.write(&[i]).expect("write") {}
                while !b2a_r.read(&mut one).expect("read") {}
            }
        },
        move |count| {
            let mut one = [0u64];
            for _ in 0..count {
                while !a2b_r.read(&mut one).expect("read") {}
                while !b2a_w.write(&one).expect("write") {}
            }
        },
    ))
}

pub fn iceoryx2(cpus: (usize, usize), count: u64) -> f64 {
    let a2b = Queue::<u64, CAPACITY>::new();
    let b2a = Queue::<u64, CAPACITY>::new();
    ping_pong(
        cpus,
        count,
        |count| {
            let mut tx = a2b.acquire_producer().expect("one producer");
            let mut rx = b2a.acquire_consumer().expect("one consumer");
            for i in 0..count {
                while !tx.push(&i) {}
                while rx.pop().is_none() {}
            }
        },
        |count| {
            let mut rx = a2b.acquire_consumer().expect("one consumer");
            let mut tx = b2a.acquire_producer().expect("one producer");
            for _ in 0..count {
                let value = loop {
                    if let Some(v) = rx.pop() {
                        break v;
                    }
                };
                while !tx.push(&value) {}
            }
        },
    )
}

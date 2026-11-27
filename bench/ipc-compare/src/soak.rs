//! A long run of one rsbinder-topic topic under churn, checked rather than timed.
//!
//! ```text
//! ipc-compare soak [--duration S] [--size B] [--rate HZ] [--n N] [--loan yes] [--backpressure MS]
//!                  [--restarts R] [--churn C] [--fault N]
//! ```
//!
//! A publisher gives the topic a name at the service manager and publishes records that carry
//! a generation, a sequence number and a body derived from both. `--n` subscribers
//! (`NamedSubscriber`) stay for the whole run and check every record: the body, that sequence
//! numbers only rise within a generation, and that `RecvError::Lagged` never counts more
//! records than the numbers say were missed. Meanwhile `--churn` more subscribers at a time
//! come and go, every other one killed (`SIGKILL`) instead of leaving. The publisher is
//! replaced `--restarts` times, killed and ended in turn, and the subscribers that stay must
//! find each successor by the name. At both ends of each publisher's life, with the same
//! subscribers attached, the driver counts the publisher's fds, memfd mappings and threads.
//!
//! The row's `failures` is everything that must be zero: records that failed the check,
//! sequence numbers out of order, over-counted losses, subscriptions refused, fds or mappings
//! that the churn left behind, and subscribers that gave no report.

use std::collections::HashMap;
use std::num::NonZeroUsize;
use std::time::Duration;

use rsbinder::*;
use rsbinder_topic::{Bytes, Closed, Disconnected, Publisher, RecvError, ServiceManager, Subscriber};

use crate::common::*;

/// Stamp, generation, and one word of body.
const SMALLEST: usize = STAMP_BYTES + 16;

/// Subscribers the publisher admits: those that stay, the churn, and the killed ones whose
/// death notice has yet to arrive.
const ADMITTED: usize = 64;

fn word(generation: u64, seq: u64) -> u64 {
    (seq ^ generation.rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1
}

fn fill(buf: &mut [u8], generation: u64, seq: u64) {
    buf[16..24].copy_from_slice(&generation.to_le_bytes());
    let word = word(generation, seq).to_le_bytes();
    for chunk in buf[24..].chunks_mut(8) {
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    stamp(buf, seq);
}

/// The record's generation and sequence number, when its body is the one they give.
fn check(buf: &[u8]) -> Option<(u64, u64)> {
    if buf.len() < SMALLEST {
        return None;
    }
    let seq = read_stamp(buf).1;
    let generation = u64::from_le_bytes(buf[16..24].try_into().ok()?);
    let word = word(generation, seq).to_le_bytes();
    buf[24..].chunks(8).all(|chunk| chunk == &word[..chunk.len()]).then_some((generation, seq))
}

/// Child role `rsb-soak-pub`: one generation of the topic's publisher. Waits for `--n`
/// subscribers, says `READY`, publishes until `--until-ns`, says `PUBLISHED <count> <failed>`.
pub fn publisher(a: &Args) -> Result<()> {
    pin(a.num("cpu", 1) as usize);
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    let size = (a.num("size", 4096) as usize).max(SMALLEST);
    let rate = a.num("rate", 0);
    let generation = a.num("gen", 0);
    let until = a.num("until-ns", 0);
    let loan = a.str("loan", "no") == "yes";
    let fault = a.num("fault", 0);
    let mut events = Publisher::<Bytes>::events().max_subscribers(ADMITTED).max_per_uid(ADMITTED);
    events = if loan {
        events.ring_bytes(64 << 10).lend(size).lend_keep(8)
    } else {
        events.ring_bytes(crate::rsb::topic_ring_for(size))
    };
    if a.has("backpressure") {
        let timeout = Duration::from_millis(a.num("backpressure", 0));
        events = match NonZeroUsize::new(a.num("window", 0) as usize) {
            Some(window) => events.lossless_window(timeout, window),
            None => events.lossless(timeout),
        };
    }
    let topic = events.build()?;
    topic.advertise_on(&a.str("name", ""), &ServiceManager)?;
    let wanted = a.num("n", 0) as usize;
    let patience = now_ns() + 10_000_000_000;
    while topic.subscriber_count() < wanted && now_ns() < patience {
        std::thread::sleep(Duration::from_millis(5));
    }
    println!("READY");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut record = Bytes(vec![0u8; size]);
    let period = if rate > 0 { 1_000_000_000 / rate } else { 0 };
    let start = now_ns();
    let (mut seq, mut failed) = (0u64, 0u64);
    loop {
        if period > 0 {
            sleep_until(start + seq * period);
        }
        if (period > 0 || seq % 256 == 0) && now_ns() >= until {
            break;
        }
        fill(&mut record.0, generation, seq);
        // `--fault N`: every Nth record is published with one byte wrong, to see the check fail.
        if fault > 0 && seq % fault == fault - 1 {
            record.0[size - 1] ^= 0x80;
        }
        let published = if loan {
            topic.loan_slice(size).and_then(|mut slot| {
                slot.copy_from_slice(&record.0);
                slot.publish()
            })
        } else {
            topic.publish(&record)
        };
        failed += u64::from(published.is_err());
        seq += 1;
    }
    println!("PUBLISHED {seq} {failed}");
    Ok(())
}

/// What one subscriber saw, as the words of its `SOAK` line.
#[derive(Default)]
struct Seen {
    received: u64,
    /// Records missed, by the sequence numbers.
    lost: u64,
    /// Records missed, by `RecvError::Lagged`.
    reported: u64,
    corrupt: u64,
    disorder: u64,
    /// Times `Lagged` had counted more than the sequence numbers then showed missing.
    overcount: u64,
    disconnects: u64,
    rejected: u64,
    generations: u64,
    evicted: u64,
    errors: u64,
}

impl Seen {
    fn line(&self) -> String {
        format!(
            "SOAK received={} lost={} reported={} corrupt={} disorder={} overcount={} \
             disconnects={} rejected={} generations={} evicted={} errors={}",
            self.received,
            self.lost,
            self.reported,
            self.corrupt,
            self.disorder,
            self.overcount,
            self.disconnects,
            self.rejected,
            self.generations,
            self.evicted,
            self.errors,
        )
    }
}

/// Child role `rsb-soak-sub`: subscribes by name, checks every record until `--until-ns`.
pub fn subscriber(a: &Args) -> Result<()> {
    pin(a.num("cpu", 2) as usize);
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();
    let until = a.num("until-ns", 0);
    let loan = a.str("loan", "no") == "yes";
    let mut subscriber = Subscriber::<Bytes>::builder()
        .max_ring_bytes(1 << 30)
        .max_pool_bytes(usize::MAX)
        .subscribe_named(&a.str("name", ""))?;
    println!("SUBSCRIBED");
    std::io::Write::flush(&mut std::io::stdout()).ok();
    let mut seen = Seen::default();
    // The generation being read, its last sequence number, and `Lagged` counts since that one.
    let mut at: Option<(u64, u64)> = None;
    let mut pending = 0u64;
    loop {
        let now = now_ns();
        if now >= until {
            break;
        }
        let wait = Duration::from_nanos((until - now).min(100_000_000));
        let got = if loan {
            subscriber.recv_sample_timeout(wait).map(|sample| sample.to_owned())
        } else {
            subscriber.recv_timeout(wait).map(|value| value.0)
        };
        match got {
            Ok(value) => {
                seen.received += 1;
                let Some((generation, seq)) = check(&value) else {
                    seen.corrupt += 1;
                    continue;
                };
                match at {
                    Some((current, last)) if current == generation => {
                        if seq <= last {
                            seen.disorder += 1;
                        } else {
                            let gap = seq - last - 1;
                            seen.lost += gap;
                            seen.overcount += u64::from(pending > gap);
                        }
                    }
                    Some((current, _)) if generation < current => seen.disorder += 1,
                    // The first record of a connection: what came before it is not counted.
                    _ => seen.generations += 1,
                }
                at = Some((generation, seq));
                pending = 0;
            }
            Err(RecvError::Lagged(n)) => {
                seen.reported += n;
                pending += n;
            }
            Err(RecvError::Timeout | RecvError::Empty) => {}
            Err(RecvError::Disconnected(why)) => {
                seen.disconnects += 1;
                if matches!(why, Disconnected::Rejected(_)) {
                    eprintln!("rsb-soak-sub: {why:?}");
                    seen.rejected += 1;
                }
                // Losses at the end of a connection have no later record to be checked against.
                pending = 0;
            }
            Err(RecvError::Closed(Closed::Evicted)) => {
                seen.evicted += 1;
                break;
            }
            Err(RecvError::Closed(why)) => {
                eprintln!("rsb-soak-sub: closed: {why:?}");
                seen.errors += 1;
                break;
            }
            Err(e) => {
                eprintln!("rsb-soak-sub: {e}");
                seen.errors += 1;
            }
        }
    }
    println!("{}", seen.line());
    Ok(())
}

fn parse(line: &str) -> HashMap<String, u64> {
    line.split(' ')
        .filter_map(|word| word.split_once('='))
        .filter_map(|(key, value)| Some((key.to_string(), value.parse().ok()?)))
        .collect()
}

/// The publisher's fds, memfd mappings and threads.
fn resources(pid: u32) -> Option<(usize, usize, usize)> {
    let entries = |dir: &str| Some(std::fs::read_dir(format!("/proc/{pid}/{dir}")).ok()?.count());
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    Some((entries("fd")?, maps.lines().filter(|l| l.contains("memfd:")).count(), entries("task")?))
}

struct Churned {
    peer: Peer,
    leaves_at: u64,
    killed: bool,
}

/// The driver: `ipc-compare soak`.
pub fn run(a: &Args) {
    let duration = a.num("duration", 600);
    let size = (a.num("size", 4096) as usize).max(SMALLEST);
    let rate = a.num("rate", 0);
    let n = a.num("n", 4) as usize;
    let restarts = a.num("restarts", 2);
    let churn = a.num("churn", 2) as usize;
    let loan = a.str("loan", "no") == "yes";
    let publisher_cpu = a.num("producer-cpu", 1).to_string();
    let cpus = a.cpus("consumer-cpus", "2,3,4,5,6,7,0,8");
    let name = crate::unique_name();
    let mut imp = if loan { "rsb-topic-loan".to_string() } else { "rsb-topic".to_string() };
    let mut shared = vec![
        ("name", name.clone()),
        ("size", size.to_string()),
        ("loan", if loan { "yes" } else { "no" }.to_string()),
    ];
    if a.has("backpressure") {
        shared.push(("backpressure", a.num("backpressure", 0).to_string()));
        shared.push(("window", a.num("window", 0).to_string()));
        imp.push_str("+bp");
    }
    if a.has("fault") {
        shared.push(("fault", a.num("fault", 0).to_string()));
    }
    let begin = now_ns();
    let end = begin + duration * 1_000_000_000;
    let mut note: Vec<String> = Vec::new();
    let mut failures = 0u64;
    let with = |extra: &[(&'static str, String)]| {
        let mut pairs = shared.clone();
        pairs.extend_from_slice(extra);
        pairs
    };

    // The subscribers that stay are there before the first publisher: they wait for the name.
    let stay_until = (end + 1_000_000_000).to_string();
    let stayers: Vec<Peer> = (0..n)
        .map(|i| {
            let cpu = cpus[i % cpus.len()].to_string();
            crate::spawn("rsb-soak-sub", &with(&[("cpu", cpu), ("until-ns", stay_until.clone())]))
        })
        .collect();
    for (i, stayer) in stayers.iter().enumerate() {
        if stayer.wait_for("SUBSCRIBED", Duration::from_secs(20)).is_none() {
            note.push(format!("subscriber {i} did not subscribe"));
            failures += 1;
        }
    }

    let generations = restarts + 1;
    let mut random = 0x2545_f491_4f6c_dd1du64;
    let mut next_random = move || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random
    };
    let (mut churned, mut killed, mut published, mut publish_failed) = (0u64, 0u64, 0u64, 0u64);
    let mut totals: HashMap<String, u64> = HashMap::new();
    let mut absorb = |line: Option<String>, failures: &mut u64, what: &str| match line {
        Some(line) => {
            for (key, value) in parse(&line) {
                *totals.entry(key).or_default() += value;
            }
        }
        None => {
            eprintln!("ipc-compare soak: {what} gave no report");
            *failures += 1;
        }
    };
    for generation in 0..generations {
        let ends_at = begin + (generation + 1) * duration * 1_000_000_000 / generations;
        let last = generation + 1 == generations;
        // Every other publisher before the last is killed; the others end their topic.
        let kill = !last && generation % 2 == 0;
        let until = if kill { u64::MAX } else { ends_at };
        let publisher = crate::spawn(
            "rsb-soak-pub",
            &with(&[
                ("cpu", publisher_cpu.clone()),
                ("rate", rate.to_string()),
                ("gen", generation.to_string()),
                ("n", n.to_string()),
                ("until-ns", until.to_string()),
            ]),
        );
        if publisher.wait_for("READY", Duration::from_secs(30)).is_none() {
            note.push(format!("publisher {generation} did not start"));
            failures += 1;
            break;
        }
        std::thread::sleep(Duration::from_millis(500));
        let before = resources(publisher.pid());
        let churn_ends = ends_at.saturating_sub(2_500_000_000);
        let mut coming: Vec<Churned> = Vec::new();
        while now_ns() < churn_ends {
            while coming.len() < churn {
                let life = 50_000_000 + next_random() % 750_000_000;
                let kill_this = churned % 2 == 1;
                let cpu = cpus[(n + churned as usize) % cpus.len()].to_string();
                // One that is killed would have stayed past the time it is killed at.
                let until = now_ns() + if kill_this { 10 * life } else { life };
                let peer =
                    crate::spawn("rsb-soak-sub", &with(&[("cpu", cpu), ("until-ns", until.to_string())]));
                coming.push(Churned { peer, leaves_at: now_ns() + life, killed: kill_this });
                churned += 1;
            }
            std::thread::sleep(Duration::from_millis(10));
            let now = now_ns();
            let (leaving, staying): (Vec<_>, Vec<_>) = coming.into_iter().partition(|c| c.leaves_at <= now);
            coming = staying;
            for gone in leaving {
                if gone.killed {
                    killed += 1;
                    drop(gone.peer);
                } else {
                    let line = gone.peer.wait_for("SOAK", Duration::from_secs(10));
                    absorb(line, &mut failures, "a subscriber that left");
                }
            }
        }
        for gone in coming {
            if gone.killed {
                killed += 1;
            } else {
                let line = gone.peer.wait_for("SOAK", Duration::from_secs(10));
                absorb(line, &mut failures, "a subscriber that left");
            }
        }
        sleep_until(ends_at.saturating_sub(500_000_000));
        let after = resources(publisher.pid());
        match (before, after) {
            (Some(b), Some(a)) => {
                note.push(format!(
                    "publisher {generation}: fds {}->{} memfd maps {}->{} threads {}->{}",
                    b.0, a.0, b.1, a.1, b.2, a.2
                ));
                failures += u64::from(b.0 != a.0) + u64::from(b.1 != a.1);
            }
            _ => {
                note.push(format!("publisher {generation}: no /proc entry"));
                failures += 1;
            }
        }
        sleep_until(ends_at);
        if kill {
            drop(publisher);
            continue;
        }
        match publisher.wait_for("PUBLISHED", Duration::from_secs(30)) {
            Some(line) => {
                let mut words = line.split(' ').skip(1).map(|w| w.parse::<u64>().unwrap_or(0));
                published += words.next().unwrap_or(0);
                publish_failed += words.next().unwrap_or(0);
            }
            None => {
                note.push(format!("publisher {generation} gave no count"));
                failures += 1;
            }
        }
        publisher.finish(Duration::from_secs(10));
    }

    let mut slowest = u64::MAX;
    let mut stayed: HashMap<String, u64> = HashMap::new();
    for (i, stayer) in stayers.iter().enumerate() {
        match stayer.wait_for("SOAK", Duration::from_secs(30)) {
            Some(line) => {
                let seen = parse(&line);
                slowest = slowest.min(seen.get("received").copied().unwrap_or(0));
                if seen.get("generations").copied().unwrap_or(0) != generations {
                    note.push(format!(
                        "subscriber {i} read {} of {generations} publishers",
                        seen.get("generations").copied().unwrap_or(0)
                    ));
                    failures += 1;
                }
                for (key, value) in seen {
                    *stayed.entry(key).or_default() += value;
                }
            }
            None => {
                note.push(format!("subscriber {i} gave no report"));
                failures += 1;
            }
        }
    }
    if slowest == u64::MAX {
        slowest = 0;
    }
    let sum = |key: &str| stayed.get(key).copied().unwrap_or(0) + totals.get(key).copied().unwrap_or(0);
    for key in ["corrupt", "disorder", "overcount", "rejected", "errors"] {
        failures += sum(key);
    }
    // Without backpressure a publish does not wait and has nothing to fail at.
    if !a.has("backpressure") {
        failures += publish_failed;
    }
    note.insert(
        0,
        format!(
            "{duration} s; {generations} publishers; {churned} subscribers came and went, {killed} \
             killed; ended publishers published {published} ({publish_failed} failed); \
             subscribers that stayed: lost {} (Lagged counted {}), disconnects {}, evicted {}; \
             all: corrupt {} disorder {} overcount {} rejected {} errors {}",
            stayed.get("lost").copied().unwrap_or(0),
            stayed.get("reported").copied().unwrap_or(0),
            stayed.get("disconnects").copied().unwrap_or(0),
            sum("evicted"),
            sum("corrupt"),
            sum("disorder"),
            sum("overcount"),
            sum("rejected"),
            sum("errors"),
        ),
    );
    Row {
        bench: "soak",
        imp: &imp,
        wait: "block",
        n,
        size,
        rate,
        count: slowest,
        warmup: 0,
        hist: None,
        mean_ns: None,
        msgs_per_s: slowest as f64 / duration.max(1) as f64,
        failures,
        note: note.join("; "),
    }
    .print();
}

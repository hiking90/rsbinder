//! Plan 15 §8: rsbinder's paths next to iceoryx2's, one process per participant, each pinned to
//! a core. Every measurement prints one CSV row (`ipc-compare header` prints the header).
//!
//! ```text
//! ipc-compare b1 --impl rsb|ix [--wait block|poll] --size B
//! ipc-compare b2 --impl rsb-stream|ix [--wait block|poll] --size B --rate HZ
//! ipc-compare b3 --impl rsb-stream|rsb-oneway|rsb-topic|ix [--wait block|poll] --n N --size B --rate HZ
//!   (rsb-topic also takes --wait fd)
//! ipc-compare b5 --wake futex|fd --n N [--rate HZ]     (one process: publish time, N waiting)
//! ipc-compare b6 --codec alloc|parcel|bytes --side publish|receive --size B  (one process: codec)
//!   (b2, b3 with --impl ix also take --touch yes: the subscriber copies the payload out)
//! ipc-compare b4 --impl fmq|ix
//!   [--count C --warmup W] [--producer-cpu 1] [--consumer-cpus 2,3,4,5,6,7,0,8]
//! ```
//!
//! `--rate 0` publishes as fast as the slowest consumer takes items. Latency is send to
//! receive (B2, B3; the send time is stamped in the payload before it is encoded) or a
//! round trip (B1); B4 reports round trip / 2 as a mean only, as iceoryx2's queue benchmark.

mod common;
mod ix;
mod queue;
mod rsb;
mod topic_cost;

use std::time::Duration;

use common::*;

const START: Duration = Duration::from_secs(20);

fn main() {
    let words: Vec<String> = std::env::args().skip(1).collect();
    let Some(what) = words.first() else {
        eprintln!("usage: ipc-compare header|b1|b2|b3|b4|child ...");
        std::process::exit(2);
    };
    if what == "child" {
        child(&words[1..]);
        return;
    }
    if what == "header" {
        println!("{HEADER}");
        return;
    }
    let a = Args::parse(&words[1..]);
    match what.as_str() {
        "b1" => b1(&a),
        "b2" | "b3" => fanout(what, &a),
        "b4" => b4(&a),
        "b5" => topic_cost::b5(&a),
        "b6" => topic_cost::b6(&a),
        _ => {
            eprintln!("ipc-compare: unknown measurement {what}");
            std::process::exit(2);
        }
    }
}

fn child(words: &[String]) {
    let Some(role) = words.first() else { std::process::exit(2) };
    let a = Args::parse(&words[1..]);
    let failed = match role.as_str() {
        "rsb-producer" => rsb::producer(&a).err().map(|e| format!("{e:?}")),
        "rsb-stream" => rsb::stream_consumer(&a).err().map(|e| format!("{e:?}")),
        "rsb-oneway" => rsb::callback_consumer(&a).err().map(|e| format!("{e:?}")),
        "rsb-topic" => rsb::topic_consumer(&a).err().map(|e| format!("{e:?}")),
        "rsb-echo" => rsb::echo_client(&a).err().map(|e| format!("{e:?}")),
        "ix-producer" => ix::producer(&a).err().map(|e| e.to_string()),
        "ix-consumer" => ix::consumer(&a).err().map(|e| e.to_string()),
        "ix-server" => ix::server(&a).err().map(|e| e.to_string()),
        "ix-client" => ix::client(&a).err().map(|e| e.to_string()),
        "ix-cleanup" => ix::cleanup().err().map(|e| e.to_string()),
        _ => Some(format!("unknown role {role}")),
    };
    if let Some(e) = failed {
        eprintln!("ipc-compare child {role}: {e}");
        println!("FAILED {e}");
        std::process::exit(1);
    }
}

fn unique_name() -> String {
    format!("ipc_compare_{}_{}", std::process::id(), now_ns())
}

fn arg_list(pairs: &[(&str, String)]) -> Vec<String> {
    pairs.iter().flat_map(|(k, v)| [format!("--{k}"), v.clone()]).collect()
}

fn spawn(role: &str, pairs: &[(&str, String)]) -> Peer {
    let mut args = vec!["child".to_string(), role.to_string()];
    args.extend(arg_list(pairs));
    Peer::spawn(&args)
}

/// Reads one RESULT from each consumer; a consumer that gives none is named in `note`.
fn collect(consumers: &[Peer], warmup: u64, timeout: Duration, note: &mut Vec<String>) -> Vec<Record> {
    let mut records = Vec::new();
    for (i, consumer) in consumers.iter().enumerate() {
        match consumer.wait_for("RESULT", timeout).and_then(|l| Record::parse(&l, warmup)) {
            Some(r) => {
                if !r.extra.is_empty() {
                    note.push(format!("consumer {i}: {}", r.extra));
                }
                records.push(r)
            }
            None => note.push(format!("consumer {i} gave no result")),
        }
    }
    records
}

fn fanout(bench: &str, a: &Args) {
    let imp = a.str("impl", "rsb-stream");
    let n = a.num("n", 1) as usize;
    let size = a.num("size", 64) as usize;
    let rate = a.num("rate", 0);
    let wait = if imp == "rsb-oneway" { "block".to_string() } else { a.str("wait", "block") };
    let (count, warmup) = match a.has("count") {
        true => (a.num("count", 0), a.num("warmup", 0)),
        false => default_count(size, rate),
    };
    let producer_cpu = a.num("producer-cpu", 1).to_string();
    let consumer_cpus = a.cpus("consumer-cpus", "2,3,4,5,6,7,0,8");
    let name = unique_name();
    let mut note = Vec::new();
    let mut failures = 0;
    let row = |hist, msgs_per_s, failures, note: Vec<String>| {
        Row {
            bench,
            imp: &imp,
            wait: &wait,
            n,
            size,
            rate,
            count,
            warmup,
            hist,
            mean_ns: None,
            msgs_per_s,
            failures,
            note: note.join("; "),
        }
        .print()
    };
    let consumer_args = |i: usize, extra: &[(&'static str, String)]| {
        let mut pairs = vec![
            ("name", name.clone()),
            ("cpu", consumer_cpus[i % consumer_cpus.len()].to_string()),
            ("count", count.to_string()),
            ("warmup", warmup.to_string()),
            ("wait", wait.clone()),
        ];
        pairs.extend_from_slice(extra);
        pairs
    };
    let (producer, role, extra) = match imp.as_str() {
        "rsb-stream" | "rsb-oneway" => {
            let ring = rsb::ring_for(size);
            if imp == "rsb-stream" {
                let largest = ring - rsbinder::stream::END_RESERVE - 4;
                if 4 + size.next_multiple_of(4) > largest {
                    let reason = format!("item exceeds the largest ring the producer accepts ({ring} B)");
                    return row(None, 0.0, 0, vec![reason]);
                }
                note.push(format!("ring {ring} B"));
            }
            let producer = spawn("rsb-producer", &[("name", name.clone()), ("cpu", producer_cpu)]);
            (producer, imp.clone(), vec![("ring", ring.to_string())])
        }
        "rsb-topic" => {
            let ring = rsb::topic_ring_for(size);
            note.push(format!("ring {ring} B; copies the payload out"));
            let producer = spawn("rsb-producer", &[("name", name.clone()), ("cpu", producer_cpu)]);
            (producer, imp.clone(), vec![("ring", ring.to_string())])
        }
        "ix" => {
            let touch = a.str("touch", "no") == "yes";
            let reads = if touch { "copies the payload out" } else { "reads the stamp in place" };
            note.push(format!("subscriber buffer {} samples; {reads}", ix::buffer_for(size)));
            let producer = spawn(
                "ix-producer",
                &[
                    ("name", name.clone()),
                    ("cpu", producer_cpu),
                    ("n", n.to_string()),
                    ("size", size.to_string()),
                    ("count", count.to_string()),
                    ("rate", rate.to_string()),
                    ("wait", wait.clone()),
                ],
            );
            let extra = if touch { vec![("touch", "yes".to_string())] } else { Vec::new() };
            (producer, "ix-consumer".to_string(), extra)
        }
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    if producer.wait_for("READY", START).is_none() {
        return row(None, 0.0, 0, vec!["producer did not start".into()]);
    }
    let mut consumers = Vec::new();
    for i in 0..n {
        let consumer = spawn(&role, &consumer_args(i, &extra));
        if consumer.wait_for("SUBSCRIBED", START).is_none() {
            return row(None, 0.0, 0, vec![format!("consumer {i} did not subscribe")]);
        }
        consumers.push(consumer);
    }
    if imp.starts_with("rsb") {
        if let Err(e) = rsb::start(&name, count, size, rate) {
            return row(None, 0.0, 0, vec![format!("start: {e:?}")]);
        }
    }
    let records = collect(&consumers, warmup, rsb::expected(count, rate), &mut note);
    if imp.starts_with("rsb") {
        failures = rsb::failures(&name).map(|f| f as u64).unwrap_or_else(|e| {
            note.push(format!("failures(): {e:?}"));
            0
        });
    }
    let (hist, msgs_per_s, received) = merge(&records);
    if received < count {
        note.push(format!("slowest consumer received {received} of {count}"));
    }
    for consumer in consumers {
        consumer.finish(Duration::from_secs(10));
    }
    // A binder producer never exits by itself; an iceoryx2 one exits after its subscribers.
    let linger = if imp == "ix" { Duration::from_secs(30) } else { Duration::ZERO };
    producer.finish(linger);
    row(Some(hist), msgs_per_s, failures, note)
}

fn b1(a: &Args) {
    let imp = a.str("impl", "rsb");
    let size = a.num("size", 8) as usize;
    let wait = if imp == "rsb" { "block".to_string() } else { a.str("wait", "block") };
    let (count, warmup) = match a.has("count") {
        true => (a.num("count", 0), a.num("warmup", 0)),
        false => {
            let count = (1_000_000_000 / size.max(1) as u64).clamp(2_000, 100_000);
            (count, count / 10)
        }
    };
    let producer_cpu = a.num("producer-cpu", 1).to_string();
    let client_cpu = a.cpus("consumer-cpus", "2")[0].to_string();
    let name = unique_name();
    let row = |hist, msgs_per_s, note: String| {
        Row {
            bench: "b1",
            imp: &imp,
            wait: &wait,
            n: 1,
            size,
            rate: 0,
            count,
            warmup,
            hist,
            mean_ns: None,
            msgs_per_s,
            failures: 0,
            note,
        }
        .print()
    };
    let (server_role, client_role) = match imp.as_str() {
        "rsb" => ("rsb-producer", "rsb-echo"),
        "ix" => ("ix-server", "ix-client"),
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    let common = |cpu: String| {
        vec![("name", name.clone()), ("cpu", cpu), ("size", size.to_string()), ("wait", wait.clone())]
    };
    let server = spawn(server_role, &common(producer_cpu));
    if server.wait_for("READY", START).is_none() {
        return row(None, 0.0, "server did not start".into());
    }
    let mut pairs = common(client_cpu);
    pairs.push(("count", count.to_string()));
    pairs.push(("warmup", warmup.to_string()));
    let client = spawn(client_role, &pairs);
    let mut note = Vec::new();
    if imp == "rsb" && size > rsbinder::blob::BLOB_INPLACE_LIMIT {
        note.push("blob through a memfd".to_string());
    }
    let records = collect(std::slice::from_ref(&client), 0, rsb::expected(count, 0), &mut note);
    let (hist, calls_per_s, received) = merge(&records);
    if received < count {
        note.push(format!("{received} of {count} calls"));
    }
    client.finish(Duration::from_secs(10));
    let linger = if imp == "ix" { Duration::from_secs(30) } else { Duration::ZERO };
    server.finish(linger);
    row(Some(hist), calls_per_s, note.join("; "));
}

fn b4(a: &Args) {
    let imp = a.str("impl", "fmq");
    let count = a.num("count", 10_000_000);
    let cpus = a.cpus("consumer-cpus", "2,3");
    let cpus = (cpus[0], cpus[1 % cpus.len()]);
    let mean = match imp.as_str() {
        "fmq" => queue::fmq(cpus, count).unwrap_or_else(|e| {
            eprintln!("ipc-compare: fmq: {e}");
            std::process::exit(1);
        }),
        "ix" => queue::iceoryx2(cpus, count),
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    Row {
        bench: "b4",
        imp: &imp,
        wait: "poll",
        n: 1,
        size: 8,
        rate: 0,
        count,
        warmup: 0,
        hist: None,
        mean_ns: Some(mean),
        msgs_per_s: 1e9 / (2.0 * mean),
        failures: 0,
        note: format!("capacity {}; round trip / 2", queue::CAPACITY),
    }
    .print();
}

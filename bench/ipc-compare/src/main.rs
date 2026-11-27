//! Plan 15 §8 and plan 20: rsbinder's paths next to iceoryx2's and D-Bus's, one process per
//! participant, each pinned to a core. Every measurement prints one CSV row (`ipc-compare
//! header` prints the header). The D-Bus rows are `zbus`'s blocking API on the bus
//! `DBUS_SESSION_BUS_ADDRESS` names.
//!
//! ```text
//! ipc-compare b1 --impl rsb|rsb-reuse|rsb-bytes|ix|dbus [--wait block|poll] --size B
//!   (rsb sends a blob, a memfd past 16 KiB; rsb-reuse the same, each side reading it into a
//!   Vec it keeps; rsb-bytes a byte[] in the transaction)
//! ipc-compare b7 --impl rsb|ix|dbus --n N --size B [--server-pin no]
//!   (N callers of one server at once; --server-pin no leaves the server's threads unpinned)
//! ipc-compare b8 --impl rsb|ix|dbus                     (a call with a struct of 8 fields)
//! ipc-compare b9 --impl rsb|ix|dbus                     (a name looked up, to a proxy)
//! ipc-compare b10 --impl rsb|rsb-state|ix|dbus|dbus-cached   (a value read)
//! ipc-compare b11 --impl rsb|dbus --size B              (a memfd handed over and mapped)
//! ipc-compare b12 --impl rsb-shm|rsb-dealer --size B    (round trips through a region kept)
//! ipc-compare soak ...                                  (src/soak.rs)
//! ipc-compare b2 --impl rsb-stream|ix [--wait block|poll] --size B --rate HZ
//! ipc-compare b3 --impl rsb-stream|rsb-oneway|rsb-topic|rsb-topic-loan|ix|dbus [--wait block|poll] --n N --size B --rate HZ
//!   (rsb-topic and rsb-topic-loan also take --wait fd; rsb-topic-loan lends a pool slot per item
//!   and its subscriber reads it in place, or copies it out with --touch yes; with --trust yes
//!   the subscriber vouches for the publisher and copies with a plain memory copy; with
//!   --backpressure MS a publish or a loan waits up to MS ms for the subscribers, and the row's
//!   impl gains `+bp`; with --window N as well, also for one it would leave more than N records
//!   behind, and the impl is `+bp-wN`; rsb-topic takes --ring B for a ring of B bytes in place
//!   of the one sized for --size)
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
mod dbus;
mod ix;
mod queue;
mod rsb;
mod soak;
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
        "b7" => b7(&a),
        "b8" => calls("b8", &a, &[("rsb", "struct"), ("ix", "struct"), ("dbus", "struct")], 0),
        "b9" => calls("b9", &a, &[("rsb", "lookup"), ("ix", "lookup"), ("dbus", "lookup")], 0),
        "b10" => {
            let ops =
                [("rsb", "value"), ("rsb-state", "state"), ("ix", "value"), ("dbus", "value"), ("dbus-cached", "cached")];
            calls("b10", &a, &ops, 8)
        }
        "b11" => calls("b11", &a, &[("rsb", "fd"), ("dbus", "fd")], a.num("size", 1 << 20) as usize),
        "b12" => {
            let ops = [("rsb-shm", "shm"), ("rsb-dealer", "dealer")];
            calls("b12", &a, &ops, a.num("size", 1 << 20) as usize)
        }
        "soak" => soak::run(&a),
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
        "rsb-topic-loan" => rsb::topic_loan_consumer(&a).err().map(|e| format!("{e:?}")),
        "rsb-echo" => rsb::echo_client(&a).err().map(|e| format!("{e:?}")),
        "rsb-call" => rsb::call_client(&a).err().map(|e| format!("{e:?}")),
        "rsb-soak-pub" => soak::publisher(&a).err().map(|e| format!("{e:?}")),
        "rsb-soak-sub" => soak::subscriber(&a).err().map(|e| format!("{e:?}")),
        "ix-call" => ix::call_client(&a).err().map(|e| e.to_string()),
        "dbus-call" => dbus::call_client(&a).err().map(|e| e.to_string()),
        "ix-producer" => ix::producer(&a).err().map(|e| e.to_string()),
        "ix-consumer" => ix::consumer(&a).err().map(|e| e.to_string()),
        "ix-server" => ix::server(&a).err().map(|e| e.to_string()),
        "ix-client" => ix::client(&a).err().map(|e| e.to_string()),
        "ix-cleanup" => ix::cleanup().err().map(|e| e.to_string()),
        "dbus-producer" => dbus::producer(&a).err().map(|e| e.to_string()),
        "dbus-consumer" => dbus::consumer(&a).err().map(|e| e.to_string()),
        "dbus-echo" => dbus::echo_client(&a).err().map(|e| e.to_string()),
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
    if size < STAMP_BYTES {
        // A value carries its stamp: below it, the rows would record a size not sent.
        eprintln!("ipc-compare: --size below the stamp's {STAMP_BYTES} bytes");
        std::process::exit(2);
    }
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
    // A topic's subscribers ask the producer for the wait when they ask for the topic.
    let backpressure = match imp.as_str() {
        "rsb-topic" | "rsb-topic-loan" if a.has("backpressure") => Some(a.num("backpressure", 0)),
        _ => None,
    };
    let window = match backpressure {
        Some(_) if a.num("window", 0) > 0 => Some(a.num("window", 0)),
        _ => None,
    };
    let label = match (backpressure, window) {
        (Some(ms), Some(records)) => {
            note.push(format!("backpressure {ms} ms window {records}"));
            format!("{imp}+bp-w{records}")
        }
        (Some(ms), None) => {
            note.push(format!("backpressure {ms} ms"));
            format!("{imp}+bp")
        }
        (None, _) => imp.clone(),
    };
    let row = |hist, msgs_per_s, failures, note: Vec<String>| {
        Row {
            bench,
            imp: &label,
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
            let ring = if a.has("ring") { a.num("ring", 0) as usize } else { rsb::topic_ring_for(size) };
            note.push(format!("ring {ring} B; copies the payload out"));
            let producer = spawn("rsb-producer", &[("name", name.clone()), ("cpu", producer_cpu)]);
            (producer, imp.clone(), vec![("ring", ring.to_string())])
        }
        "rsb-topic-loan" => {
            // The pool keeps as many values as iceoryx2's subscriber buffer for this size.
            let depth = ix::buffer_for(size);
            let slot = size.max(STAMP_BYTES);
            let touch = a.str("touch", "no") == "yes";
            let trust = a.str("trust", "no") == "yes";
            let reads = if touch { "copies the payload out" } else { "reads the stamp in place" };
            let vouches = if trust { "; trusts the publisher" } else { "" };
            note.push(format!("pool {} slots of {slot} B; {reads}{vouches}", n + 1 + depth + 1));
            let producer = spawn("rsb-producer", &[("name", name.clone()), ("cpu", producer_cpu)]);
            let mut extra = vec![
                ("ring", (64 << 10).to_string()),
                ("slot", slot.to_string()),
                ("depth", depth.to_string()),
                ("n", n.to_string()),
            ];
            if touch {
                extra.push(("touch", "yes".to_string()));
            }
            if trust {
                extra.push(("trust", "yes".to_string()));
            }
            (producer, imp.clone(), extra)
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
        "dbus" => {
            note.push("signal through dbus-broker; no flow control".to_string());
            let producer = spawn("dbus-producer", &[("name", name.clone()), ("cpu", producer_cpu)]);
            (producer, "dbus-consumer".to_string(), Vec::new())
        }
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    let mut extra = extra;
    if let Some(ms) = backpressure {
        extra.push(("backpressure", ms.to_string()));
    }
    if let Some(records) = window {
        extra.push(("window", records.to_string()));
    }
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
    if imp == "dbus" {
        if let Err(e) = dbus::start(&name, count, size, rate) {
            return row(None, 0.0, 0, vec![format!("start: {e}")]);
        }
    }
    let records = collect(&consumers, warmup, rsb::expected(count, rate), &mut note);
    if imp.starts_with("rsb") {
        failures = rsb::failures(&name).map(|f| f as u64).unwrap_or_else(|e| {
            note.push(format!("failures(): {e:?}"));
            0
        });
    }
    if imp == "dbus" {
        failures = dbus::failures(&name).map(|f| f as u64).unwrap_or_else(|e| {
            note.push(format!("failures(): {e}"));
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
    let wait = if imp == "ix" { a.str("wait", "block") } else { "block".to_string() };
    let (count, warmup) = match a.has("count") {
        true => (a.num("count", 0), a.num("warmup", 0)),
        false => {
            let count = (1_000_000_000 / size.max(1) as u64).clamp(200, 100_000);
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
        // `rsb-bytes`: the payload as an AIDL `byte[]`, in the transaction whatever its size.
        // `rsb-reuse`: a blob each side reads into a `Vec` it keeps (`Blob::read_into`).
        "rsb" | "rsb-bytes" | "rsb-reuse" => ("rsb-producer", "rsb-echo"),
        "ix" => ("ix-server", "ix-client"),
        "dbus" => ("dbus-producer", "dbus-echo"),
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    let payload = match imp.as_str() {
        "rsb-bytes" => "bytes",
        "rsb-reuse" => "blob-reuse",
        _ => "blob",
    };
    let common = |cpu: String| {
        vec![
            ("name", name.clone()),
            ("cpu", cpu),
            ("size", size.to_string()),
            ("wait", wait.clone()),
            ("payload", payload.to_string()),
        ]
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
    if matches!(imp.as_str(), "rsb" | "rsb-reuse") && size > rsbinder::blob::BLOB_INPLACE_LIMIT {
        note.push("blob through a memfd".to_string());
    }
    if imp == "rsb-reuse" {
        note.push("each side reads into a Vec it keeps".to_string());
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

/// B7: `--n` callers of one server at once, each making `--count` round trips of `--size`
/// bytes. The row's rate is the sum of the callers', and its histogram all their round trips.
fn b7(a: &Args) {
    let imp = a.str("impl", "rsb");
    let n = a.num("n", 4) as usize;
    let size = a.num("size", 64) as usize;
    let wait = if imp == "ix" { a.str("wait", "block") } else { "block".to_string() };
    let count = a.num("count", 20_000);
    let warmup = a.num("warmup", count / 10);
    let server_cpu = a.num("producer-cpu", 1).to_string();
    let client_cpus = a.cpus("consumer-cpus", "2,3,4,5,6,7,0,8");
    let name = unique_name();
    let row = |hist, calls_per_s, note: String| {
        Row {
            bench: "b7",
            imp: &imp,
            wait: &wait,
            n,
            size,
            rate: 0,
            count,
            warmup,
            hist,
            mean_ns: None,
            msgs_per_s: calls_per_s,
            failures: 0,
            note,
        }
        .print()
    };
    let (server_role, client_role) = match imp.as_str() {
        "rsb" => ("rsb-producer", "rsb-echo"),
        "ix" => ("ix-server", "ix-client"),
        "dbus" => ("dbus-producer", "dbus-echo"),
        other => {
            eprintln!("ipc-compare: unknown --impl {other}");
            std::process::exit(2);
        }
    };
    let common = |cpu: String| {
        vec![
            ("name", name.clone()),
            ("cpu", cpu),
            ("size", size.to_string()),
            ("wait", wait.clone()),
            ("n", n.to_string()),
        ]
    };
    // `--server-pin no`: the server's threads go where the scheduler puts them.
    let server_free = a.str("server-pin", "yes") == "no";
    let mut server_pairs = common(server_cpu);
    if server_free {
        server_pairs.push(("pin", "no".to_string()));
    }
    let server = spawn(server_role, &server_pairs);
    if server.wait_for("READY", START).is_none() {
        return row(None, 0.0, "server did not start".into());
    }
    // Long enough for every caller to connect before the first call.
    let start_at = now_ns() + 2_000_000_000 + n as u64 * 100_000_000;
    let mut clients = Vec::new();
    for i in 0..n {
        let mut pairs = common(client_cpus[i % client_cpus.len()].to_string());
        pairs.push(("count", count.to_string()));
        pairs.push(("warmup", warmup.to_string()));
        pairs.push(("start-at", start_at.to_string()));
        let client = spawn(client_role, &pairs);
        if client.wait_for("CONNECTED", START).is_none() {
            return row(None, 0.0, format!("caller {i} did not connect"));
        }
        clients.push(client);
    }
    let server_where = if server_free { "server not pinned" } else { "server on one cpu" };
    let mut note = vec![format!("{server_where}; {n} callers on {} cpus", client_cpus.len().min(n))];
    if now_ns() > start_at {
        note.push("callers connected after the start time".to_string());
    }
    let records = collect(&clients, 0, rsb::expected(count, 0), &mut note);
    let (hist, calls_per_s, received) = merge_sum(&records);
    if received < count * n as u64 {
        note.push(format!("{received} of {} calls", count * n as u64));
    }
    for client in clients {
        client.finish(Duration::from_secs(10));
    }
    let linger = if imp == "ix" { Duration::from_secs(30) } else { Duration::ZERO };
    server.finish(linger);
    row(Some(hist), calls_per_s, note.join("; "));
}

/// B8 to B11: one server and one client that makes `--count` calls of one kind. `ops` maps each
/// `--impl` the measurement has to the `--op` its client runs; the family before a `-` picks
/// the processes.
fn calls(bench: &str, a: &Args, ops: &[(&str, &str)], size: usize) {
    let imp = a.str("impl", "rsb");
    let Some((_, op)) = ops.iter().find(|(known, _)| *known == imp) else {
        eprintln!("ipc-compare: {bench} has no --impl {imp}");
        std::process::exit(2);
    };
    let family = imp.split('-').next().unwrap_or_default();
    let wait = if family == "ix" { a.str("wait", "block") } else { "block".to_string() };
    let count = match *op {
        "lookup" => 5_000,
        // B12 copies its payload four times a call, as B1 does: B1's count, by size.
        "shm" | "dealer" => (1_000_000_000 / size.max(1) as u64).clamp(200, 50_000),
        _ => 50_000,
    };
    let count = a.num("count", count);
    let warmup = a.num("warmup", count / 10);
    let name = unique_name();
    let row = |hist, calls_per_s, note: String| {
        Row {
            bench,
            imp: &imp,
            wait: &wait,
            n: 1,
            size,
            rate: 0,
            count,
            warmup,
            hist,
            mean_ns: None,
            msgs_per_s: calls_per_s,
            failures: 0,
            note,
        }
        .print()
    };
    let common = |cpu: String| {
        vec![
            ("name", name.clone()),
            ("cpu", cpu),
            ("size", size.to_string()),
            ("wait", wait.clone()),
            ("op", op.to_string()),
            // B9's iceoryx2 client holds one port open and opens another per call.
            ("n", "2".to_string()),
        ]
    };
    let (server_role, client_role) = match family {
        "rsb" => ("rsb-producer", "rsb-call"),
        "ix" => ("ix-server", "ix-call"),
        _ => ("dbus-producer", "dbus-call"),
    };
    let server = spawn(server_role, &common(a.num("producer-cpu", 1).to_string()));
    if server.wait_for("READY", START).is_none() {
        return row(None, 0.0, "server did not start".into());
    }
    let mut pairs = common(a.cpus("consumer-cpus", "2")[0].to_string());
    pairs.push(("count", count.to_string()));
    pairs.push(("warmup", warmup.to_string()));
    let client = spawn(client_role, &pairs);
    let what = match (*op, family) {
        ("struct", "ix") => "a repr(C) struct of fixed arrays; no encoding",
        ("struct", "rsb") => "a parcelable of 8 fields: string; string[4]; int[16]; nested",
        ("struct", _) => "(ixdbsasai(iid))",
        ("lookup", "rsb") => "service manager lookup to a proxy",
        ("lookup", "ix") => "service opened by name and a client port made",
        ("lookup", _) => "GetNameOwner and a proxy",
        ("value", "rsb") => "a call",
        ("value", "ix") => "blackboard entry read",
        ("value", _) => "Properties.Get",
        ("state", _) => "state topic: try_latest; the value held when none is newer",
        ("cached", _) => "the proxy's property cache",
        ("fd", "rsb") => "memfd as a ParcelFileDescriptor; the server maps and unmaps it",
        ("fd", _) => "memfd as an h; the server maps and unmaps it",
        ("shm", _) => "one SharedMemory shared before; each side copies in and out",
        ("dealer", _) => "an IMemory of a MemoryDealer each way, heaps kept mapped; each side copies in and out",
        _ => "",
    };
    let mut note = vec![what.to_string()];
    let records = collect(std::slice::from_ref(&client), 0, rsb::expected(count, 0), &mut note);
    let (hist, calls_per_s, received) = merge(&records);
    if received < count {
        note.push(format!("{received} of {count} calls"));
    }
    client.finish(Duration::from_secs(10));
    let linger = if family == "ix" { Duration::from_secs(30) } else { Duration::ZERO };
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

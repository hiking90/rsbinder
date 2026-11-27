//! Many services called by many clients at once over kernel binder, timed and checked.
//!
//! ```text
//! ipc-compare load [--servers S] [--services K] [--threads T] [--clients C] [--ops LIST]
//!                  [--size B] [--blob B] [--duration SEC] [--warmup SEC] [--churn N]
//!                  [--probe-rate HZ] [--rounds R] [--wrap CMD] [--fault N]
//! ```
//!
//! The driver starts `--servers` processes, each serving `--services` `ILoad` services under a
//! binder thread ceiling of `--threads`. `--clients` processes then call every service for
//! `--duration` seconds, each cycling through `--ops` (default all six) in a closed loop and
//! picking the service at random:
//!
//! - `echo`: a `byte[]` of `--size` bytes and back, in the transaction;
//! - `blob`: `--blob` bytes and back through `Parcel::write_blob` (a memfd past 16 KiB);
//! - `post`: a oneway of `--size` bytes the service checks and counts;
//! - `callback`: the service calls the client's own binder from inside the call, a transaction
//!   nested in the client's (the client runs no thread pool, so only the thread that waits can
//!   take it);
//! - `relay`: the service calls `echo` on its twin in the next server process, so server threads
//!   wait on other servers; the chain only runs downstream, since a cycle of saturated pools
//!   deadlocks in any binder implementation;
//! - `lookup`: a name looked up at the service manager.
//!
//! Every client also links the death of its binder at one service (`watch`). Meanwhile
//! `--churn` more clients at a time come and go, every other one killed (`SIGKILL`), most likely
//! inside a call. One more client, the probe, makes `echo` calls at `--probe-rate` beside the
//! load; `--clients 0 --churn 0` gives the probe's row without it.
//!
//! `--rounds R` runs all of that R times with the same servers. Before the first round and
//! after each, once the posts and deaths on their way have arrived and no client is left, the
//! driver takes a snapshot of every server and of `rsb_hub`: fds, memfd mappings, threads,
//! resident memory, and, when binderfs is mounted with `stats=global`, the driver's count of
//! the process's transaction buffers, nodes and references (`binder_logs/stats`). For a server
//! it also reads its live heap allocations and bytes (src/heap.rs) and the highest proxy handle
//! a client's binder came in on. `--wrap CMD` runs each server under CMD, `{i}` in it replaced
//! by the server's index (`--wrap 'heaptrack -r -o DIR/server-{i}'`: `-r` keeps heaptrack from
//! opening its GUI when the server exits, and leaves a raw file for `heaptrack_interpret`); at
//! the end the driver asks
//! every server to `quit()`, so that the tool writes its report.
//!
//! Each op gives a row of the last round's loaded clients' round trips past `--warmup`, and
//! `probe` the probe's. The `check` row's `failures` is everything that must be zero: replies
//! that differ from the request, calls that failed (a oneway refused for want of async space is
//! counted in the note instead), posts that never arrived, deaths not delivered once per watch,
//! a server's or the hub's fds, memfd mappings, buffers, nodes or references that differ
//! between the first snapshot and the last, and processes that gave no report. Its note has
//! every snapshot's values in order. `--fault N` makes every service answer every Nth echo or
//! blob with one byte wrong and leave every Nth post uncounted, to see the check fail.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use rsbinder::*;

use crate::common::*;
use crate::rsb::generated::ipccompare::ILoad::{BnLoad, ILoad};
use crate::rsb::generated::ipccompare::ILoadSink::{BnLoadSink, ILoadSink};
use crate::rsb::BlobBytes;

/// Stamp, tag, and one word of body.
const SMALLEST: usize = STAMP_BYTES + 16;

const OPS: [&str; 6] = ["echo", "blob", "post", "callback", "relay", "lookup"];

/// The driver's per-process counts; binderfs writes it when mounted with `stats=global`.
const BINDER_STATS: &str = "/dev/binderfs/binder_logs/stats";

fn word(tag: u64, seq: u64) -> u64 {
    (seq ^ tag.rotate_left(32)).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1
}

fn fill(buf: &mut [u8], tag: u64, seq: u64) {
    buf[16..24].copy_from_slice(&tag.to_le_bytes());
    let word = word(tag, seq).to_le_bytes();
    for chunk in buf[24..].chunks_mut(8) {
        chunk.copy_from_slice(&word[..chunk.len()]);
    }
    stamp(buf, seq);
}

/// Whether the body is the one the payload's tag and sequence number give.
fn intact(buf: &[u8]) -> bool {
    if buf.len() < SMALLEST {
        return false;
    }
    let seq = read_stamp(buf).1;
    let tag = u64::from_le_bytes(buf[16..24].try_into().expect("8 bytes"));
    let word = word(tag, seq).to_le_bytes();
    buf[24..].chunks(8).all(|chunk| chunk == &word[..chunk.len()])
}

fn service_name(prefix: &str, process: u64, service: u64) -> String {
    format!("{prefix}_{process}_{service}")
}

fn ready(word: &str) {
    println!("{word}");
    std::io::stdout().flush().ok();
}

fn connect(name: &str) -> Result<Strong<dyn ILoad>> {
    rsbinder::connect(Uri::kernel().with_service(name))
}

/// One service's counters and the death links it holds.
#[derive(Default)]
struct State {
    posted: AtomicI64,
    corrupt: AtomicI64,
    relay_failed: AtomicI64,
    callback_failed: AtomicI64,
    watched: AtomicI64,
    deaths: AtomicI64,
    next: AtomicU64,
    links: Mutex<HashMap<u64, DeathLink>>,
    /// Watches whose death arrived; their links are dropped outside the death callback.
    dead: Mutex<Vec<u64>>,
}

impl State {
    /// Drops the links of watches that died; returns the watches still waiting for a death.
    fn sweep(&self) -> i64 {
        let dead = std::mem::take(&mut *self.dead.lock().expect("lock"));
        let mut links = self.links.lock().expect("lock");
        for id in dead {
            links.remove(&id);
        }
        links.len() as i64
    }
}

struct Obituary {
    id: u64,
    state: Weak<State>,
}

impl DeathRecipient for Obituary {
    fn binder_died(&self, _who: &WIBinder) {
        if let Some(state) = self.state.upgrade() {
            state.deaths.fetch_add(1, Ordering::Relaxed);
            state.dead.lock().expect("lock").push(self.id);
        }
    }
}

struct LoadService {
    state: Arc<State>,
    downstream: Option<String>,
    relay_to: Mutex<Option<Arc<Strong<dyn ILoad>>>>,
    /// `--fault N`: every Nth echo, blob or post is answered with one byte wrong or not counted,
    /// to see the check fail.
    fault: u64,
    calls: AtomicU64,
}

impl LoadService {
    fn faulty(&self) -> bool {
        self.fault > 0 && self.calls.fetch_add(1, Ordering::Relaxed) % self.fault == self.fault - 1
    }

    fn answer(&self, data: &[u8]) -> Vec<u8> {
        let mut reply = data.to_vec();
        if self.faulty() {
            if let Some(last) = reply.last_mut() {
                *last ^= 0x80;
            }
        }
        reply
    }

    fn downstream(&self) -> BinderResult<Arc<Strong<dyn ILoad>>> {
        let mut cached = self.relay_to.lock().expect("lock");
        if let Some(target) = &*cached {
            return Ok(target.clone());
        }
        let name = self.downstream.as_deref().ok_or(StatusCode::InvalidOperation)?;
        let target = Arc::new(connect(name)?);
        *cached = Some(target.clone());
        Ok(target)
    }
}

impl Interface for LoadService {}

impl ILoad for LoadService {
    fn r#echo(&self, data: &[u8]) -> BinderResult<Vec<u8>> {
        Ok(self.answer(data))
    }

    fn r#echoBlob(&self, data: &BlobBytes) -> BinderResult<BlobBytes> {
        Ok(BlobBytes(self.answer(&data.0)))
    }

    fn r#post(&self, data: &[u8]) -> BinderResult<()> {
        if self.faulty() {
            return Ok(());
        }
        let counter = if intact(data) { &self.state.posted } else { &self.state.corrupt };
        counter.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }

    fn r#callBack(&self, sink: &Strong<dyn ILoadSink>, data: &[u8]) -> BinderResult<Vec<u8>> {
        note_handle(sink);
        sink.r#echo(data).inspect_err(|_| {
            self.state.callback_failed.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn r#relay(&self, data: &[u8]) -> BinderResult<Vec<u8>> {
        self.downstream().and_then(|target| target.r#echo(data)).inspect_err(|_| {
            self.state.relay_failed.fetch_add(1, Ordering::Relaxed);
        })
    }

    fn r#watch(&self, sink: &Strong<dyn ILoadSink>) -> BinderResult<()> {
        note_handle(sink);
        let state = &self.state;
        let id = state.next.fetch_add(1, Ordering::Relaxed);
        let obituary = Arc::new(Obituary { id, state: Arc::downgrade(state) });
        // Inserted under the lock `sweep` takes, so a death that arrives first is still swept.
        let mut links = state.links.lock().expect("lock");
        let link = DeathLink::new(&sink.as_binder(), obituary)?;
        state.watched.fetch_add(1, Ordering::Relaxed);
        links.insert(id, link);
        drop(links);
        state.sweep();
        Ok(())
    }

    fn r#counters(&self) -> BinderResult<Vec<i64>> {
        let live = self.state.sweep();
        let (allocs, bytes) = crate::heap::live();
        let s = &self.state;
        let get = |counter: &AtomicI64| counter.load(Ordering::Relaxed);
        Ok(vec![
            get(&s.posted),
            get(&s.corrupt),
            get(&s.relay_failed),
            get(&s.callback_failed),
            get(&s.watched),
            get(&s.deaths),
            live,
            allocs,
            bytes,
            MAX_HANDLE.load(Ordering::Relaxed),
        ])
    }

    fn r#quit(&self) -> BinderResult<()> {
        std::process::exit(0)
    }
}

/// The highest proxy handle a sink has come in on, in this process. Both drivers give a new
/// reference the lowest free handle, so this stays near the number of references held at once
/// unless references are never released.
static MAX_HANDLE: AtomicI64 = AtomicI64::new(0);

fn note_handle(sink: &Strong<dyn ILoadSink>) {
    if let Some(proxy) = sink.as_binder().as_proxy() {
        MAX_HANDLE.fetch_max(i64::from(proxy.handle()), Ordering::Relaxed);
    }
}

/// Child role `rsb-load-server`: server process `--index` of `--servers`, serving `--services`
/// services until killed or `quit()`. Says `READY <pid>`: run under a tool, the driver's child
/// is the tool.
pub fn server(a: &Args) -> Result<()> {
    let prefix = a.str("name", "");
    let index = a.num("index", 0);
    let servers = a.num("servers", 1);
    let threads = a.num("threads", 4) as u32;
    let fault = a.num("fault", 0);
    let mut server = rsbinder::serve(Uri::kernel())?.with(|o| o.threads = Some(threads));
    for k in 0..a.num("services", 1) {
        let downstream = (index + 1 < servers).then(|| service_name(&prefix, index + 1, k));
        let service = LoadService {
            state: Arc::default(),
            downstream,
            relay_to: Mutex::new(None),
            fault,
            calls: AtomicU64::new(0),
        };
        server = server.add(&service_name(&prefix, index, k), BnLoad::new_binder(service).as_binder())?;
    }
    let _server = server.spawn()?;
    ready(&format!("READY {}", std::process::id()));
    ProcessState::join_thread_pool()
}

struct EchoSink;

impl Interface for EchoSink {}

impl ILoadSink for EchoSink {
    fn r#echo(&self, data: &[u8]) -> BinderResult<Vec<u8>> {
        Ok(data.to_vec())
    }
}

/// Child role `rsb-load-client`: calls the services until `--until-ns`, says `LOAD <op> RESULT
/// ...` for each op (unless `--report check`), then `CHECK ...` with its counts.
pub fn client(a: &Args) -> Result<()> {
    let prefix = a.str("name", "");
    let prefix = prefix.as_str();
    let servers = a.num("servers", 1);
    let services = a.num("services", 1);
    let ops_arg = a.str("ops", "echo");
    let ops: Vec<&str> = ops_arg.split(',').collect();
    let id = a.num("id", 0);
    let until = a.num("until-ns", 0);
    let measure_from = a.num("measure-ns", 0);
    let rate = a.num("rate", 0);
    let names: Vec<String> =
        (0..servers).flat_map(|i| (0..services).map(move |k| service_name(prefix, i, k))).collect();
    let targets = names.iter().map(|name| connect(name)).collect::<Result<Vec<_>>>()?;
    // The services that have a twin downstream: all but the last server process's.
    let relaying = (services * servers.saturating_sub(1)) as usize;
    let sink = BnLoadSink::new_binder(EchoSink);
    if a.str("watch", "no") == "yes" {
        targets[id as usize % targets.len()].r#watch(&sink)?;
    }
    let mut small = vec![0u8; (a.num("size", 64) as usize).max(SMALLEST)];
    let mut big = BlobBytes(vec![0u8; (a.num("blob", 256 << 10) as usize).max(SMALLEST)]);
    let mut records: Vec<Record> = ops.iter().map(|_| Record::new(0)).collect();
    let mut errors = vec![0u64; ops.len()];
    let mut corrupt = vec![0u64; ops.len()];
    let mut posted = 0u64;
    let mut random = (id + 1).wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    let mut pick = move |range: usize| {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        (random % range.max(1) as u64) as usize
    };
    wait_for_start(a);
    // `--run-ms`: a span from here, once connected, in place of `--until-ns`.
    let until = if a.has("run-ms") { now_ns() + a.num("run-ms", 0) * 1_000_000 } else { until };
    let period = 1_000_000_000u64.checked_div(rate).unwrap_or(0);
    let start = now_ns();
    let mut seq = 0u64;
    loop {
        if period > 0 {
            sleep_until(start + seq * period);
        }
        if now_ns() >= until {
            break;
        }
        let slot = (seq % ops.len() as u64) as usize;
        let tag = (id << 8) | slot as u64;
        let began = now_ns();
        let outcome: BinderResult<bool> = match ops[slot] {
            "echo" => {
                fill(&mut small, tag, seq);
                targets[pick(targets.len())].r#echo(&small).map(|reply| reply == small)
            }
            "blob" => {
                fill(&mut big.0, tag, seq);
                targets[pick(targets.len())].r#echoBlob(&big).map(|reply| reply.0 == big.0)
            }
            "post" => {
                fill(&mut small, tag, seq);
                targets[pick(targets.len())].r#post(&small).map(|()| true)
            }
            "callback" => {
                fill(&mut small, tag, seq);
                targets[pick(targets.len())].r#callBack(&sink, &small).map(|reply| reply == small)
            }
            "relay" => {
                fill(&mut small, tag, seq);
                targets[pick(relaying)].r#relay(&small).map(|reply| reply == small)
            }
            "lookup" => match rsbinder::hub::check_service(&names[pick(names.len())]) {
                Some(_) => Ok(true),
                None => Err(StatusCode::NameNotFound.into()),
            },
            other => {
                eprintln!("rsb-load-client: unknown op {other}");
                std::process::exit(2);
            }
        };
        let ended = now_ns();
        match outcome {
            Ok(true) => {
                posted += u64::from(ops[slot] == "post");
                let record = &mut records[slot];
                record.received += 1;
                if began >= measure_from {
                    if record.first_ns == 0 {
                        record.first_ns = began;
                    }
                    record.hist.saturating_record((ended - began).max(1));
                    record.last_ns = ended;
                }
            }
            Ok(false) => corrupt[slot] += 1,
            Err(_) => errors[slot] += 1,
        }
        seq += 1;
    }
    if a.str("report", "all") == "all" {
        for (op, record) in ops.iter().zip(&records) {
            println!("LOAD {op} {}", record.line());
        }
    }
    let mut check = format!("CHECK posted={posted}");
    for (slot, op) in ops.iter().enumerate() {
        check.push_str(&format!(" errors.{op}={} corrupt.{op}={}", errors[slot], corrupt[slot]));
    }
    ready(&check);
    Ok(())
}

fn parse(line: &str) -> HashMap<String, u64> {
    line.split(' ')
        .filter_map(|word| word.split_once('='))
        .filter_map(|(key, value)| Some((key.to_string(), value.parse().ok()?)))
        .collect()
}

/// A process's fds, memfd mappings, threads and resident kB.
fn resources(pid: u32) -> Option<[usize; 4]> {
    let (fds, maps, threads) = crate::soak::resources(pid)?;
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let rss = status.lines().find_map(|l| l.strip_prefix("VmRSS:"))?.trim().trim_end_matches(" kB");
    Some([fds, maps, threads, rss.parse().ok()?])
}

/// What the binder driver holds for one process: transaction buffers, nodes, references and
/// their strong and weak counts.
#[derive(Clone, Copy, PartialEq)]
struct Binder {
    buffers: u64,
    nodes: u64,
    refs: u64,
    strong: u64,
    weak: u64,
}

/// The `proc <pid>` block of `binder_logs/stats`. Both drivers print `nodes: N`,
/// `refs: N s S w W` and `buffers: N` in it; `None` when any of the three is missing.
fn binder_stats(stats: &str, pid: u32) -> Option<Binder> {
    let header = format!("proc {pid}");
    let (mut buffers, mut nodes, mut refs) = (None, None, None);
    for line in stats.lines().skip_while(|line| *line != header).skip(1) {
        if line.starts_with("proc ") {
            break;
        }
        let line = line.trim();
        if let Some(value) = line.strip_prefix("buffers:") {
            buffers = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("nodes:") {
            nodes = value.trim().parse().ok();
        } else if let Some(value) = line.strip_prefix("refs:") {
            let words: Vec<&str> = value.split_whitespace().collect();
            if let [count, "s", strong, "w", weak] = words[..] {
                refs = Some((count.parse().ok()?, strong.parse().ok()?, weak.parse().ok()?));
            }
        }
    }
    let (refs, strong, weak) = refs?;
    Some(Binder { buffers: buffers?, nodes: nodes?, refs, strong, weak })
}

/// One process at one snapshot.
#[derive(Clone, Copy)]
struct Proc {
    fds: usize,
    maps: usize,
    threads: usize,
    rss_kb: usize,
    /// A server's live heap allocations, live heap bytes, and highest sink handle.
    heap: Option<(i64, i64, i64)>,
    binder: Option<Binder>,
}

fn proc_of(pid: u32, stats: Option<&str>) -> Option<Proc> {
    let [fds, maps, threads, rss_kb] = resources(pid)?;
    Some(Proc { fds, maps, threads, rss_kb, heap: None, binder: stats.and_then(|s| binder_stats(s, pid)) })
}

fn hub_pid() -> Option<u32> {
    std::fs::read_dir("/proc")
        .ok()?
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<u32>().ok())
        .find(|pid| std::fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == "rsb_hub"))
}

/// Every server, then the hub.
struct Snapshot {
    servers: Vec<Option<Proc>>,
    hub: Option<Proc>,
}

/// `counters` is each service's `counters()`, in the order the services are numbered.
fn snapshot(pids: &[u32], counters: Option<&Vec<Vec<i64>>>, services: usize, hub: Option<u32>) -> Snapshot {
    let stats = std::fs::read_to_string(BINDER_STATS).ok();
    let servers = pids
        .iter()
        .enumerate()
        .map(|(i, &pid)| {
            let mut proc = proc_of(pid, stats.as_deref())?;
            proc.heap = counters.and_then(|all| all.get(i * services)).map(|c| (c[7], c[8], c[9]));
            Some(proc)
        })
        .collect();
    Snapshot { servers, hub: hub.and_then(|pid| proc_of(pid, stats.as_deref())) }
}

/// One process's values over the snapshots, as the note shows them, and the failures they give:
/// a process missing from one, or fds, memfd mappings, buffers, nodes or references that the
/// last snapshot does not have as many of as the first.
fn report(who: &str, series: &[Option<Proc>]) -> (String, u64) {
    if series.iter().any(Option::is_none) {
        return (format!("{who}: no /proc entry at a snapshot"), 1);
    }
    let procs: Vec<Proc> = series.iter().flatten().copied().collect();
    let line = |name: &str, value: &dyn Fn(&Proc) -> Option<String>| {
        let values: Vec<String> = procs.iter().map(|p| value(p).unwrap_or_else(|| "-".into())).collect();
        format!("{name} {}", values.join("->"))
    };
    let mut parts = vec![
        line("fds", &|p| Some(p.fds.to_string())),
        line("memfd maps", &|p| Some(p.maps.to_string())),
        line("threads", &|p| Some(p.threads.to_string())),
        line("rss kB", &|p| Some(p.rss_kb.to_string())),
    ];
    if procs.iter().any(|p| p.heap.is_some()) {
        parts.push(line("heap allocs", &|p| p.heap.map(|h| h.0.to_string())));
        parts.push(line("heap bytes", &|p| p.heap.map(|h| h.1.to_string())));
        parts.push(line("max sink handle", &|p| p.heap.map(|h| h.2.to_string())));
    }
    if procs.iter().any(|p| p.binder.is_some()) {
        parts.push(line("binder buffers", &|p| p.binder.map(|b| b.buffers.to_string())));
        parts.push(line("nodes", &|p| p.binder.map(|b| b.nodes.to_string())));
        parts.push(line("refs", &|p| p.binder.map(|b| format!("{}(s{}w{})", b.refs, b.strong, b.weak))));
    }
    let (first, last) = (procs[0], procs[procs.len() - 1]);
    let mut failures = u64::from(first.fds != last.fds) + u64::from(first.maps != last.maps);
    if let (Some(b), Some(a)) = (first.binder, last.binder) {
        failures += u64::from(b != a);
    }
    (format!("{who}: {}", parts.join(", ")), failures)
}

struct Churned {
    peer: Peer,
    leaves_at: u64,
    killed: bool,
}

/// Counts every client that reported sums into one table; one that did not is a failure.
#[derive(Default)]
struct Tally {
    sums: HashMap<String, u64>,
    missing: Vec<String>,
}

impl Tally {
    fn absorb(&mut self, line: Option<String>, who: &str) {
        match line {
            Some(line) => {
                for (key, value) in parse(&line) {
                    *self.sums.entry(key).or_default() += value;
                }
            }
            None => self.missing.push(format!("{who} gave no report")),
        }
    }

    fn add(&mut self, other: &Tally) {
        for (key, value) in &other.sums {
            *self.sums.entry(key.clone()).or_default() += value;
        }
        self.missing.extend(other.missing.iter().cloned());
    }

    fn get(&self, key: &str) -> u64 {
        self.sums.get(key).copied().unwrap_or(0)
    }
}

/// What every round runs: the clients' arguments and how many of each.
struct Plan {
    shared: Vec<(&'static str, String)>,
    ops: Vec<String>,
    ops_arg: String,
    /// The clients that come and go do not post: a killed one could not say what it sent.
    churn_ops: String,
    clients: usize,
    churn: usize,
    probe_rate: u64,
    duration: u64,
    warmup: u64,
}

impl Plan {
    fn with(&self, extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut pairs = self.shared.clone();
        pairs.extend_from_slice(extra);
        pairs
    }
}

/// What one round gave: each op's records from the clients that stayed, and the probe's.
struct Round {
    by_op: Vec<Vec<Record>>,
    stayers: Tally,
    probe: Option<Record>,
    probe_counts: Tally,
    churned: u64,
    killed: u64,
}

/// One round of load. Every client's counts go into `tally` as well.
fn round(plan: &Plan, tally: &mut Tally, note: &mut Vec<String>, next_random: &mut impl FnMut() -> u64) -> Round {
    let start_at = now_ns() + 1_000_000_000 + plan.clients as u64 * 50_000_000;
    let until = start_at + plan.duration * 1_000_000_000;
    let measure_from = start_at + plan.warmup * 1_000_000_000;
    let timing = [
        ("start-at", start_at.to_string()),
        ("until-ns", until.to_string()),
        ("measure-ns", measure_from.to_string()),
    ];
    let mut stayers = Vec::new();
    for i in 0..plan.clients {
        let mut pairs =
            plan.with(&[("ops", plan.ops_arg.clone()), ("id", i.to_string()), ("watch", "yes".to_string())]);
        pairs.extend_from_slice(&timing);
        let peer = crate::spawn("rsb-load-client", &pairs);
        if peer.wait_for("CONNECTED", crate::START).is_none() {
            tally.missing.push(format!("client {i} did not connect"));
            continue;
        }
        stayers.push(peer);
    }
    let probe = (plan.probe_rate > 0).then(|| {
        let mut pairs = plan.with(&[
            ("ops", "echo".to_string()),
            ("id", "1000000".to_string()),
            ("rate", plan.probe_rate.to_string()),
        ]);
        pairs.extend_from_slice(&timing);
        crate::spawn("rsb-load-client", &pairs)
    });
    if let Some(probe) = &probe {
        if probe.wait_for("CONNECTED", crate::START).is_none() {
            tally.missing.push("the probe did not connect".to_string());
        }
    }

    let (mut churned, mut killed) = (0u64, 0u64);
    let mut coming: Vec<Churned> = Vec::new();
    let churn_ends = until.saturating_sub(1_000_000_000);
    while plan.churn > 0 && now_ns() < churn_ends {
        while coming.len() < plan.churn {
            let life = 50_000_000 + next_random() % 750_000_000;
            let kill_this = churned % 2 == 1;
            // One that is killed would have stayed past the time it is killed at.
            let leaves = now_ns() + if kill_this { 10 * life } else { life };
            let peer = crate::spawn(
                "rsb-load-client",
                &plan.with(&[
                    ("ops", plan.churn_ops.clone()),
                    ("id", (10_000 + churned).to_string()),
                    ("watch", "yes".to_string()),
                    ("until-ns", leaves.to_string()),
                    ("report", "check".to_string()),
                ]),
            );
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
                tally.absorb(gone.peer.wait_for("CHECK", Duration::from_secs(20)), "a client that left");
            }
        }
    }
    for gone in coming {
        if gone.killed {
            killed += 1;
        } else {
            tally.absorb(gone.peer.wait_for("CHECK", Duration::from_secs(20)), "a client that left");
        }
    }

    sleep_until(until);
    let patience = Duration::from_secs(60);
    let mut by_op: Vec<Vec<Record>> = plan.ops.iter().map(|_| Vec::new()).collect();
    let mut stayer_counts = Tally::default();
    for (i, stayer) in stayers.iter().enumerate() {
        for (slot, op) in plan.ops.iter().enumerate() {
            let record = stayer
                .wait_for("LOAD ", patience)
                .and_then(|line| line.strip_prefix(&format!("LOAD {op} ")).and_then(|r| Record::parse(r, 0)));
            match record {
                Some(record) => by_op[slot].push(record),
                None => note.push(format!("client {i} gave no {op} result")),
            }
        }
        stayer_counts.absorb(stayer.wait_for("CHECK", patience), &format!("client {i}"));
    }
    let mut probe_record = None;
    let mut probe_counts = Tally::default();
    if let Some(probe) = &probe {
        probe_record =
            probe.wait_for("LOAD echo ", patience).and_then(|l| Record::parse(l.strip_prefix("LOAD echo ")?, 0));
        if probe_record.is_none() {
            note.push("the probe gave no result".to_string());
        }
        probe_counts.absorb(probe.wait_for("CHECK", patience), "the probe");
    }
    for stayer in stayers {
        stayer.finish(Duration::from_secs(10));
    }
    if let Some(probe) = probe {
        probe.finish(Duration::from_secs(10));
    }
    tally.add(&stayer_counts);
    tally.add(&probe_counts);
    Round { by_op, stayers: stayer_counts, probe: probe_record, probe_counts, churned, killed }
}

/// Every service's `counters()` once the posts the clients sent have arrived and every watch's
/// death has, or after `patience` with what there is; `None` when a service does not answer.
fn settle(targets: &[Strong<dyn ILoad>], posted: u64, patience: Duration) -> Option<Vec<Vec<i64>>> {
    let end = now_ns() + patience.as_nanos() as u64;
    loop {
        let read: Option<Vec<Vec<i64>>> =
            targets.iter().map(|t| t.r#counters().ok().filter(|c| c.len() >= 10)).collect();
        let settled = read.as_ref().is_some_and(|all| {
            let arrived: i64 = all.iter().map(|c| c[0] + c[1]).sum();
            arrived as u64 == posted && all.iter().all(|c| c[4] == c[5] && c[6] == 0)
        });
        if settled || now_ns() >= end {
            return read;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The driver: `ipc-compare load`.
pub fn run(a: &Args) {
    let servers = a.num("servers", 2).max(1);
    let services = a.num("services", 2).max(1);
    let threads = a.num("threads", 4).max(1);
    let clients = a.num("clients", 8) as usize;
    let ops_arg = a.str("ops", &OPS.join(","));
    let ops: Vec<String> = ops_arg.split(',').map(String::from).collect();
    if let Some(op) = ops.iter().find(|op| !OPS.contains(&op.as_str())) {
        eprintln!("ipc-compare load: unknown op {op}; the ops are {}", OPS.join(","));
        std::process::exit(2);
    }
    if ops.iter().any(|op| op == "relay") && servers < 2 {
        eprintln!("ipc-compare load: relay needs --servers 2 or more");
        std::process::exit(2);
    }
    let size = (a.num("size", 64) as usize).max(SMALLEST);
    let blob = (a.num("blob", 256 << 10) as usize).max(SMALLEST);
    let duration = a.num("duration", 30).max(2);
    let warmup = a.num("warmup", 2).min(duration - 1);
    let churn = a.num("churn", 2) as usize;
    let rounds = a.num("rounds", 1).max(1);
    let wrap: Vec<String> = a.str("wrap", "").split_whitespace().map(String::from).collect();
    // A server under a tool starts and answers slower.
    let patience = if wrap.is_empty() { Duration::from_secs(15) } else { Duration::from_secs(120) };
    let name = crate::unique_name();
    let plan = Plan {
        shared: vec![
            ("name", name.clone()),
            ("servers", servers.to_string()),
            ("services", services.to_string()),
            ("size", size.to_string()),
            ("blob", blob.to_string()),
        ],
        churn_ops: ops.iter().filter(|op| *op != "post").cloned().collect::<Vec<_>>().join(","),
        ops,
        ops_arg,
        clients,
        churn,
        probe_rate: a.num("probe-rate", 1000),
        duration,
        warmup,
    };
    let mut config = format!(
        "{servers} servers x {services} services, thread ceiling {threads}; {clients} clients, \
         churn {churn}; {rounds} rounds of {duration} s past {warmup} s; payload {size} B, blob \
         {blob} B; not pinned"
    );
    if !wrap.is_empty() {
        config.push_str(&format!("; servers under {}", wrap.join(" ")));
    }
    let mut note: Vec<String> = Vec::new();
    let mut tally = Tally::default();
    let check_row = |failures: u64, count: u64, note: Vec<String>| {
        Row {
            bench: "load",
            imp: "check",
            wait: "block",
            n: clients,
            size,
            rate: 0,
            count,
            warmup: 0,
            hist: None,
            mean_ns: None,
            msgs_per_s: 0.0,
            failures,
            note: note.join("; "),
        }
        .print()
    };

    let mut server_peers = Vec::new();
    let mut pids = Vec::new();
    for i in 0..servers {
        let wrap: Vec<String> = wrap.iter().map(|w| w.replace("{i}", &i.to_string())).collect();
        let peer = crate::spawn_under(
            &wrap,
            "rsb-load-server",
            &plan.with(&[
                ("index", i.to_string()),
                ("threads", threads.to_string()),
                ("fault", a.num("fault", 0).to_string()),
            ]),
        );
        let Some(line) = peer.wait_for("READY", crate::START.max(patience)) else {
            return check_row(1, 0, vec![config, format!("server {i} did not start")]);
        };
        pids.push(line.split(' ').nth(1).and_then(|pid| pid.parse().ok()).unwrap_or(peer.pid()));
        server_peers.push(peer);
    }
    let prefix = name.as_str();
    let names: Vec<String> =
        (0..servers).flat_map(|i| (0..services).map(move |k| service_name(prefix, i, k))).collect();
    let targets = match names.iter().map(|n| connect(n)).collect::<Result<Vec<_>>>() {
        Ok(targets) => targets,
        Err(e) => return check_row(1, 0, vec![config, format!("driver connect: {e:?}")]),
    };
    let hub = hub_pid();

    // One short client first, so that whatever a server makes on its first calls is there
    // before the first snapshot.
    let warm = crate::spawn(
        "rsb-load-client",
        &plan.with(&[
            ("ops", plan.ops_arg.clone()),
            ("id", "999999".to_string()),
            ("run-ms", "500".to_string()),
            ("report", "check".to_string()),
        ]),
    );
    tally.absorb(warm.wait_for("CHECK", crate::START.max(patience)), "the warm-up client");
    warm.finish(Duration::from_secs(10));
    std::thread::sleep(Duration::from_millis(300));
    let first = settle(&targets, tally.get("posted"), patience);
    let mut snapshots = vec![snapshot(&pids, first.as_ref(), services as usize, hub)];

    let mut random = 0x2545_f491_4f6c_dd1du64;
    let mut next_random = move || {
        random ^= random << 13;
        random ^= random >> 7;
        random ^= random << 17;
        random
    };
    let (mut churned, mut killed) = (0, 0);
    let mut last = None;
    let mut counters = None;
    for _ in 0..rounds {
        let this = round(&plan, &mut tally, &mut note, &mut next_random);
        churned += this.churned;
        killed += this.killed;
        counters = settle(&targets, tally.get("posted"), patience);
        snapshots.push(snapshot(&pids, counters.as_ref(), services as usize, hub));
        last = Some(this);
    }
    let last = last.expect("one round at least");

    let rows_note = |extra: &str| if extra.is_empty() { config.clone() } else { format!("{config}; {extra}") };
    let mut count = 0;
    for (slot, op) in plan.ops.iter().enumerate() {
        let (hist, calls_per_s, _) = merge_sum(&last.by_op[slot]);
        count += hist.len();
        let errors = if op == "post" { 0 } else { last.stayers.get(&format!("errors.{op}")) };
        Row {
            bench: "load",
            imp: op,
            wait: "block",
            n: clients,
            size: if op == "blob" { blob } else { size },
            rate: 0,
            count: hist.len(),
            warmup: 0,
            hist: Some(hist),
            mean_ns: None,
            msgs_per_s: calls_per_s,
            failures: errors + last.stayers.get(&format!("corrupt.{op}")),
            note: rows_note(if op == "post" { "send time of a oneway; last round" } else { "last round" }),
        }
        .print();
    }
    if let Some(record) = &last.probe {
        let (hist, calls_per_s, _) = merge_sum(std::slice::from_ref(record));
        Row {
            bench: "load",
            imp: "probe",
            wait: "block",
            n: clients,
            size,
            rate: plan.probe_rate,
            count: hist.len(),
            warmup: 0,
            hist: Some(hist),
            mean_ns: None,
            msgs_per_s: calls_per_s,
            failures: last.probe_counts.get("errors.echo") + last.probe_counts.get("corrupt.echo"),
            note: rows_note("one caller of echo at the probe rate beside the load; last round"),
        }
        .print();
    }

    let mut summary = vec![config.clone()];
    let mut failures = tally.missing.len() as u64;
    summary.extend(tally.missing.iter().cloned());
    let (mut client_errors, mut client_corrupt) = (0, 0);
    for op in OPS {
        if op != "post" {
            client_errors += tally.get(&format!("errors.{op}"));
        }
        client_corrupt += tally.get(&format!("corrupt.{op}"));
    }
    failures += client_errors + client_corrupt;
    summary.push(format!(
        "{churned} clients came and went, {killed} killed; client calls failed {client_errors}, \
         replies differed {client_corrupt}, oneways refused {}",
        tally.get("errors.post")
    ));
    match &counters {
        Some(all) => {
            let sum = |i: usize| all.iter().map(|c| c[i]).sum::<i64>();
            let posted = tally.get("posted");
            let arrived = (sum(0) + sum(1)) as u64;
            failures += sum(1) as u64 + posted.abs_diff(arrived);
            let unmatched = all.iter().filter(|c| c[4] != c[5] || c[6] != 0).count() as u64;
            failures += unmatched;
            summary.push(format!(
                "servers: posts sent {posted} arrived {arrived} corrupt {}; relays failed {}, \
                 callbacks failed {}; watches {} deaths {} undelivered {}; services with a death \
                 count off {unmatched}",
                sum(1),
                sum(2),
                sum(3),
                sum(4),
                sum(5),
                sum(6)
            ));
        }
        None => {
            failures += 1;
            summary.push("a service did not answer counters()".to_string());
        }
    }
    if !snapshots.iter().any(|s| s.servers.iter().flatten().any(|p| p.binder.is_some())) {
        summary.push(format!("no {BINDER_STATS}: binderfs is not mounted with stats=global"));
    }
    for i in 0..pids.len() {
        let series: Vec<_> = snapshots.iter().map(|s| s.servers[i]).collect();
        let (line, failed) = report(&format!("server {i}"), &series);
        summary.push(line);
        failures += failed;
    }
    if hub.is_some() {
        let series: Vec<_> = snapshots.iter().map(|s| s.hub).collect();
        let (line, failed) = report("rsb_hub", &series);
        summary.push(line);
        failures += failed;
    }
    summary.extend(note);
    check_row(failures, count, summary);

    for i in 0..servers as usize {
        let _ = targets[i * services as usize].r#quit();
    }
    for (peer, pid) in server_peers.into_iter().zip(pids) {
        peer.finish(if wrap.is_empty() { Duration::from_secs(10) } else { Duration::from_secs(300) });
        // Under a tool, the peer is the tool; its server must not outlive the run.
        let _ = rustix::process::kill_process(
            rustix::process::Pid::from_raw(pid as i32).expect("a pid"),
            rustix::process::Signal::KILL,
        );
    }
}

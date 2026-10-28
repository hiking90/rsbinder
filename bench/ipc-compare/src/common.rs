//! What every measurement shares: the clock, core pinning, the stamp in a payload, the latency
//! record a consumer sends back to the driver, and the child processes the driver runs.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use hdrhistogram::serialization::{Deserializer, Serializer, V2Serializer};
use hdrhistogram::Histogram;

/// Bytes at the head of every payload: send time (ns), then sequence number.
pub const STAMP_BYTES: usize = 16;

/// A consumer that hears nothing for this long reports what it has.
pub const SILENCE: Duration = Duration::from_secs(10);

pub fn now_ns() -> u64 {
    let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
}

/// Pins the calling thread; threads it starts afterwards inherit the mask.
pub fn pin(cpu: usize) {
    let mut set = rustix::thread::CpuSet::new();
    set.set(cpu);
    if let Err(e) = rustix::thread::sched_setaffinity(None, &set) {
        eprintln!("ipc-compare: pinning to cpu {cpu}: {e}");
        std::process::exit(1);
    }
}

pub fn stamp(buf: &mut [u8], seq: u64) {
    if buf.len() >= STAMP_BYTES {
        buf[8..16].copy_from_slice(&seq.to_le_bytes());
        buf[0..8].copy_from_slice(&now_ns().to_le_bytes());
    }
}

pub fn read_stamp(buf: &[u8]) -> (u64, u64) {
    let word = |at: usize| u64::from_le_bytes(buf[at..at + 8].try_into().expect("8 bytes"));
    (word(0), word(8))
}

/// Sleeps until `deadline` (ns of `now_ns`), then returns.
pub fn sleep_until(deadline: u64) {
    let now = now_ns();
    if deadline > now {
        std::thread::sleep(Duration::from_nanos(deadline - now));
    }
}

pub fn histogram() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 120_000_000_000, 3).expect("histogram bounds")
}

/// What one consumer measured: latencies past the warmup, and the span they arrived over.
pub struct Record {
    pub hist: Histogram<u64>,
    pub warmup: u64,
    pub received: u64,
    /// Send time of the first measured item.
    pub first_ns: u64,
    /// Receive time of the last item.
    pub last_ns: u64,
    /// A remark for the CSV note, one word (no spaces).
    pub extra: String,
}

impl Record {
    pub fn new(warmup: u64) -> Self {
        Record { hist: histogram(), warmup, received: 0, first_ns: 0, last_ns: 0, extra: String::new() }
    }

    pub fn take(&mut self, payload: &[u8]) {
        let now = now_ns();
        let (sent, seq) = read_stamp(payload);
        self.received += 1;
        if seq >= self.warmup {
            if self.first_ns == 0 {
                self.first_ns = sent;
            }
            self.hist.saturating_record(now.saturating_sub(sent).max(1));
            self.last_ns = now;
        }
    }

    pub fn line(&self) -> String {
        let mut bytes = Vec::new();
        V2Serializer::new().serialize(&self.hist, &mut bytes).expect("serialize histogram");
        let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
        let line = format!("RESULT {} {} {} {hex}", self.received, self.first_ns, self.last_ns);
        if self.extra.is_empty() { line } else { format!("{line} {}", self.extra) }
    }

    pub fn parse(line: &str, warmup: u64) -> Option<Self> {
        let mut words = line.strip_prefix("RESULT ")?.split(' ');
        let received = words.next()?.parse().ok()?;
        let first_ns = words.next()?.parse().ok()?;
        let last_ns = words.next()?.parse().ok()?;
        let hex = words.next()?;
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16))
            .collect::<Result<_, _>>()
            .ok()?;
        let hist = Deserializer::new().deserialize(&mut bytes.as_slice()).ok()?;
        let extra = words.next().unwrap_or_default().to_string();
        Some(Record { hist, warmup, received, first_ns, last_ns, extra })
    }
}

/// One CSV row. Throughput is the slowest consumer's.
pub struct Row<'a> {
    pub bench: &'a str,
    pub imp: &'a str,
    pub wait: &'a str,
    pub n: usize,
    pub size: usize,
    pub rate: u64,
    pub count: u64,
    pub warmup: u64,
    pub hist: Option<Histogram<u64>>,
    /// A mean measured without a histogram (B4).
    pub mean_ns: Option<f64>,
    pub msgs_per_s: f64,
    pub failures: u64,
    pub note: String,
}

pub const HEADER: &str = "bench,impl,wait,n,size,rate_hz,count,warmup,p50_ns,p99_ns,p999_ns,max_ns,\
mean_ns,msgs_per_s,mb_per_s,failures,note";

impl Row<'_> {
    pub fn print(&self) {
        let q = |h: &Histogram<u64>, p: f64| h.value_at_quantile(p).to_string();
        let (p50, p99, p999, max, mut mean) = match &self.hist {
            Some(h) if !h.is_empty() => (
                q(h, 0.5),
                q(h, 0.99),
                q(h, 0.999),
                h.max().to_string(),
                format!("{:.0}", h.mean()),
            ),
            _ => Default::default(),
        };
        if let Some(m) = self.mean_ns {
            mean = format!("{m:.1}");
        }
        let mb = self.msgs_per_s * self.size as f64 / 1e6;
        println!(
            "{},{},{},{},{},{},{},{},{p50},{p99},{p999},{max},{mean},{:.0},{:.1},{},{}",
            self.bench,
            self.imp,
            self.wait,
            self.n,
            self.size,
            self.rate,
            self.count,
            self.warmup,
            self.msgs_per_s,
            mb,
            self.failures,
            self.note.replace(',', ";"),
        );
    }
}

/// Merges consumer records into one row's histogram and slowest throughput.
pub fn merge(records: &[Record]) -> (Histogram<u64>, f64, u64) {
    let mut hist = histogram();
    let mut slowest = f64::INFINITY;
    let mut received = u64::MAX;
    for r in records {
        hist.add(&r.hist).expect("same bounds");
        let span = r.last_ns.saturating_sub(r.first_ns) as f64 / 1e9;
        let measured = r.hist.len() as f64;
        slowest = slowest.min(if span > 0.0 { measured / span } else { 0.0 });
        received = received.min(r.received);
    }
    if records.is_empty() {
        slowest = 0.0;
        received = 0;
    }
    (hist, slowest, received)
}

/// `--key value` arguments.
pub struct Args(HashMap<String, String>);

impl Args {
    pub fn parse(words: &[String]) -> Self {
        let mut map = HashMap::new();
        let mut it = words.iter();
        while let Some(word) = it.next() {
            let Some(key) = word.strip_prefix("--") else {
                eprintln!("ipc-compare: unexpected argument {word}");
                std::process::exit(2);
            };
            let value = it.next().cloned().unwrap_or_default();
            map.insert(key.to_string(), value);
        }
        Args(map)
    }

    pub fn str(&self, key: &str, default: &str) -> String {
        self.0.get(key).cloned().unwrap_or_else(|| default.to_string())
    }

    pub fn num(&self, key: &str, default: u64) -> u64 {
        match self.0.get(key) {
            Some(v) => v.parse().unwrap_or_else(|_| {
                eprintln!("ipc-compare: --{key} {v} is not a number");
                std::process::exit(2);
            }),
            None => default,
        }
    }

    pub fn has(&self, key: &str) -> bool {
        self.0.contains_key(key)
    }

    pub fn cpus(&self, key: &str, default: &str) -> Vec<usize> {
        self.str(key, default)
            .split(',')
            .map(|c| c.parse().expect("cpu number"))
            .collect()
    }
}

/// A child process of this binary, its stdout read line by line on a thread.
pub struct Peer {
    child: Child,
    lines: mpsc::Receiver<String>,
}

impl Peer {
    pub fn spawn(args: &[String]) -> Self {
        let exe = std::env::current_exe().expect("current_exe");
        let mut child = Command::new(exe)
            .args(args)
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn child");
        let stdout = child.stdout.take().expect("piped stdout");
        let (tx, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        Peer { child, lines }
    }

    /// The first line that starts with `prefix`; `None` at timeout or when the child exits.
    pub fn wait_for(&self, prefix: &str, timeout: Duration) -> Option<String> {
        let end = Instant::now() + timeout;
        loop {
            let left = end.checked_duration_since(Instant::now())?;
            match self.lines.recv_timeout(left) {
                Ok(line) if line.starts_with(prefix) => return Some(line),
                Ok(line) => eprintln!("child: {line}"),
                Err(_) => return None,
            }
        }
    }
}

impl Peer {
    /// Waits up to `timeout` for the child to exit by itself, then kills it.
    pub fn finish(self, timeout: Duration) {
        let mut peer = self;
        let end = Instant::now() + timeout;
        while Instant::now() < end {
            match peer.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            }
        }
        if !timeout.is_zero() {
            eprintln!("ipc-compare: child {} did not exit; killing it", peer.child.id());
        }
    }
}

impl Drop for Peer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Items for a run when `--count` is not given: about 3 s at a fixed rate, and a byte budget
/// at full speed.
pub fn default_count(size: usize, rate: u64) -> (u64, u64) {
    let by_bytes = (2_000_000_000 / size.max(1) as u64).clamp(1_000, 200_000);
    let count = if rate > 0 { (rate * 3).min(by_bytes) } else { by_bytes };
    (count, (count / 10).max(100))
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! `rsb_service topic`: a topic as a process outside its publisher sees it — its status, the rate
//! of its records, the records themselves, and a file of them.
//!
//! Each command finds the topic by its name once, through one of three sources: `rsb_hub`'s topic
//! registry (the default), the service manager (`--service`), or the registry a hub serves on a
//! Unix socket (`--topic-socket`). It never guesses: a name the chosen source has no topic for
//! is exit 1.
//!
//! `hz`, `echo` and `record` subscribe once as a subscriber of any type
//! (`rsbinder_topic::AnyType`) that the publisher does not wait for
//! (`SubscriberBuilder::unwaited`): a slow terminal holds up no subscriber the topic serves, and
//! loses records of its own instead, which each command reports. The subscription counts against
//! the topic's bounds, so a full topic refuses it.
//!
//! # The file `record` writes
//!
//! Little-endian. A header: the magic `RSTR`, a `u32` format version (1), the `u32` topic kind
//! (`ITopic.TOPIC_EVENT` or `TOPIC_STATE`), the `u32` length of the type name and its bytes.
//! Then one entry per record: a `u64` receive time (`CLOCK_MONOTONIC` nanoseconds), a `u32`
//! length and the payload; or, where records were lost, the length `0xFFFF_FFFF` and a `u64`
//! count of them. Each entry is flushed as it is written.

use std::fmt::Write as _;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use rsbinder::{hub, BinderResult, ExceptionCode, SIBinder, Status, StatusCode, Strong};
use rsbinder_topic::aidl::ITopic::{ITopic, TOPIC_EVENT, TOPIC_STATE};
use rsbinder_topic::aidl::TopicStatus::TopicStatus;
use rsbinder_topic::registry::RegistryResolver;
use rsbinder_topic::{AnyType, RecvError, Start, Subscriber, SubscriberBuilder, TopicKind};

/// The exit codes of `rsb_service`: answered, the name has no topic, not answerable.
const OK: u8 = 0;
const NO: u8 = 1;
const ERR: u8 = 2;

/// The magic and format version at the head of a `record` file.
pub const RECORD_MAGIC: &[u8; 4] = b"RSTR";
pub const RECORD_VERSION: u32 = 1;
/// The length that marks an entry of lost records.
pub const RECORD_LOST: u32 = u32::MAX;

/// Where a command looks the topic's name up.
#[derive(Clone, Debug)]
pub enum Source {
    /// The topic registry `rsb_hub` serves with its `topic` feature.
    Registry,
    /// The service manager: a topic registered as a service.
    Service,
    /// The registry a hub serves on this Unix socket (`rsb_hub --topic-socket`).
    Socket(PathBuf),
}

/// What a watching command does with the records.
#[derive(Clone, Debug)]
pub enum Watch {
    /// Each second, the records received, their intervals and those lost.
    Hz,
    /// Each record's length and bytes.
    Echo,
    /// Each record into this file.
    Record(PathBuf),
}

/// The topic a name has, as its source answers.
enum Found {
    Binder(SIBinder),
    #[cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]
    Socket(rsbinder_topic::registry::uds::UdsResolver),
}

/// `Err` with the exit code once the reason is printed.
fn find(name: &str, source: &Source) -> Result<Found, u8> {
    let found = match source {
        Source::Registry => {
            let resolver = RegistryResolver::find().map_err(|status| {
                eprintln!(
                    "rsb_service: no topic registry: {status} (rsb_hub serves one with its \
                     `topic` feature, to callers its policy lets find it)"
                );
                ERR
            })?;
            match resolver.lookup(name) {
                Ok(found) => found,
                Err(status) => return Err(failed("ITopicRegistry.lookup", &status)),
            }
        }
        Source::Service => hub::check_service(name),
        Source::Socket(path) => return on_socket(path),
    };
    found.map(Found::Binder).ok_or_else(|| {
        eprintln!("rsb_service: {name}: no topic (or not one this caller may reach)");
        NO
    })
}

#[cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]
fn on_socket(path: &std::path::Path) -> Result<Found, u8> {
    use rsbinder_topic::registry::uds::{ResolverOptions, UdsResolver};
    use rsbinder_topic::uds::Endpoint;
    let endpoint = Endpoint::Path(path.to_owned());
    match UdsResolver::connect(endpoint, &ResolverOptions::default()) {
        Ok(resolver) => Ok(Found::Socket(resolver)),
        Err(status) => {
            eprintln!(
                "rsb_service: no topic registry on {}: {status}",
                path.display()
            );
            Err(ERR)
        }
    }
}

#[cfg(not(all(feature = "rpc", any(target_os = "linux", target_os = "android"))))]
fn on_socket(path: &std::path::Path) -> Result<Found, u8> {
    eprintln!(
        "rsb_service: {} takes the features `topic` and `rpc`, on Linux or Android; this build \
         is not that",
        path.display()
    );
    Err(ERR)
}

/// The reason a call failed, with where to look for a refusal; exit 1 for a name with no topic.
fn failed(what: &str, status: &Status) -> u8 {
    eprintln!("rsb_service: {what} failed: {status}");
    match status.exception_code() {
        ExceptionCode::Security => {
            eprintln!(
                "  The publisher's `PublisherBuilder::inspect` decides who may look, and its \
                 `authorize` who may subscribe; the hub's policy who may find the name."
            );
            ERR
        }
        ExceptionCode::TransactionFailed
            if status.transaction_error() == StatusCode::NameNotFound =>
        {
            NO
        }
        _ => ERR,
    }
}

/// `topic stat`: what `ITopic.status` answers, or exit 2 when no answer comes within `timeout`.
pub fn stat(name: &str, source: &Source, timeout: Duration) -> u8 {
    let found = match find(name, source) {
        Ok(found) => found,
        Err(code) => return code,
    };
    let (sent, answered) = mpsc::channel();
    let asked = name.to_owned();
    // A kernel binder call has no deadline: one to a stopped publisher waits for ever.
    let asking = std::thread::Builder::new()
        .name("rsb_service-stat".into())
        .spawn(move || {
            let _ = sent.send(ask_status(&asked, found));
        });
    if asking.is_err() {
        eprintln!("rsb_service: cannot start the thread that asks");
        return ERR;
    }
    match answered.recv_timeout(timeout) {
        Ok(Ok(status)) => {
            print!("{status}");
            OK
        }
        Ok(Err(status)) => failed("ITopic.status", &status),
        // The thread is left waiting; the process ends with it.
        Err(_) => {
            eprintln!("rsb_service: {name}: no answer within {timeout:?} (is the publisher stopped?)");
            ERR
        }
    }
}

fn ask_status(name: &str, found: Found) -> BinderResult<TopicStatus> {
    match found {
        Found::Binder(binder) => {
            let _ = name;
            let topic: Strong<dyn ITopic> = binder.into_interface()?;
            topic.r#status()
        }
        #[cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]
        Found::Socket(resolver) => resolver.status(name),
    }
}

/// `topic hz`, `echo` and `record`: subscribe once, then receive `count` records or until the
/// topic ends.
pub fn watch(name: &str, source: &Source, watch: &Watch, count: Option<u64>) -> u8 {
    let start = match watch {
        Watch::Echo => Start::Auto,
        Watch::Hz | Watch::Record(_) => Start::Next,
    };
    let builder = Subscriber::<AnyType>::builder().unwaited().start_at(start);
    let subscribed = match find(name, source) {
        Ok(found) => subscribe(name, found, builder),
        Err(code) => return code,
    };
    let subscriber = match subscribed {
        Ok(subscriber) => subscriber,
        Err(status) => return failed("the subscription", &status),
    };
    let sink: Box<dyn Sink> = match watch {
        Watch::Hz => Box::new(Rate::new()),
        Watch::Echo => Box::new(Echo::new(&subscriber)),
        Watch::Record(path) => match Recorder::create(path, &subscriber) {
            Ok(recorder) => Box::new(recorder),
            Err(error) => {
                eprintln!("rsb_service: cannot write {}: {error}", path.display());
                return ERR;
            }
        },
    };
    receive(subscriber, sink, count)
}

fn subscribe(
    name: &str,
    found: Found,
    builder: SubscriberBuilder<AnyType>,
) -> BinderResult<Subscriber<AnyType>> {
    match found {
        Found::Binder(binder) => {
            // The publisher's death and the end of its records reach a pool thread.
            rsbinder::ProcessState::start_thread_pool();
            let _ = name;
            builder.subscribe(&binder)
        }
        #[cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]
        Found::Socket(resolver) => resolver.subscribe(name, builder),
    }
}

/// What a watching command does with each thing a receive returns.
trait Sink {
    fn value(&mut self, bytes: &[u8]) -> std::io::Result<()>;
    fn lost(&mut self, count: u64) -> std::io::Result<()>;
    /// A receive that waited and got nothing.
    fn idle(&mut self) -> std::io::Result<()> {
        Ok(())
    }
    /// The last call: the subscription ended or the count was reached.
    fn finish(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn receive(mut subscriber: Subscriber<AnyType>, mut sink: Box<dyn Sink>, count: Option<u64>) -> u8 {
    let mut received = 0;
    let ended = loop {
        if count.is_some_and(|count| received >= count) {
            break None;
        }
        let step = match subscriber.recv_timeout(Duration::from_millis(100)) {
            Ok(AnyType(bytes)) => {
                received += 1;
                sink.value(&bytes)
            }
            Err(RecvError::Lagged(lost)) => sink.lost(lost),
            Err(RecvError::Timeout | RecvError::Empty) => sink.idle(),
            Err(other) => break Some(other),
        };
        if let Err(error) = step {
            eprintln!("rsb_service: cannot write: {error}");
            return ERR;
        }
    };
    if let Err(error) = sink.finish() {
        eprintln!("rsb_service: cannot write: {error}");
        return ERR;
    }
    match ended {
        None => OK,
        Some(error @ RecvError::Closed(_)) => {
            eprintln!("rsb_service: {error}");
            OK
        }
        Some(error) => {
            eprintln!("rsb_service: the subscription failed: {error}");
            ERR
        }
    }
}

/// Records per second, and the intervals between them.
struct Rate {
    window: Instant,
    last: Option<Instant>,
    count: u64,
    lost: u64,
    intervals: Vec<Duration>,
}

impl Rate {
    fn new() -> Self {
        Self {
            window: Instant::now(),
            last: None,
            count: 0,
            lost: 0,
            intervals: Vec::new(),
        }
    }

    /// Print the window once a second has passed, or `always`.
    fn report(&mut self, always: bool) -> std::io::Result<()> {
        let span = self.window.elapsed();
        if span < Duration::from_secs(1) && !always {
            return Ok(());
        }
        let mut line = format!(
            "{} records in {:.2}s ({:.1} Hz)",
            self.count,
            span.as_secs_f64(),
            self.count as f64 / span.as_secs_f64().max(f64::MIN_POSITIVE)
        );
        let ms = |d: &Duration| d.as_secs_f64() * 1e3;
        if let (Some(min), Some(max)) = (self.intervals.iter().min(), self.intervals.iter().max()) {
            let total: Duration = self.intervals.iter().sum();
            let mean = total.as_secs_f64() * 1e3 / self.intervals.len() as f64;
            let _ = write!(
                line,
                ", interval mean {mean:.3}ms min {:.3}ms max {:.3}ms",
                ms(min),
                ms(max)
            );
        }
        let _ = write!(line, ", lost {}", self.lost);
        writeln!(std::io::stdout(), "{line}")?;
        self.window = Instant::now();
        self.count = 0;
        self.lost = 0;
        self.intervals.clear();
        Ok(())
    }
}

impl Sink for Rate {
    fn value(&mut self, _: &[u8]) -> std::io::Result<()> {
        let now = Instant::now();
        if let Some(last) = self.last.replace(now) {
            self.intervals.push(now - last);
        }
        self.count += 1;
        self.report(false)
    }

    fn lost(&mut self, count: u64) -> std::io::Result<()> {
        self.lost += count;
        self.report(false)
    }

    fn idle(&mut self) -> std::io::Result<()> {
        self.report(false)
    }

    fn finish(&mut self) -> std::io::Result<()> {
        self.report(true)
    }
}

/// The type name `Bytes` has, whose records may be text.
const BYTES: &str = "bytes";
/// The bytes of a record `echo` prints in hex; past them it says how many more there are.
const ECHOED: usize = 256;

struct Echo {
    text: bool,
}

impl Echo {
    fn new(subscriber: &Subscriber<AnyType>) -> Self {
        println!(
            "type {:?}, {} topic",
            subscriber.topic_type(),
            kind_name(subscriber.topic_kind())
        );
        Self {
            text: subscriber.topic_type() == BYTES,
        }
    }
}

impl Sink for Echo {
    fn value(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let mut line = format!("{} bytes:", bytes.len());
        for byte in bytes.iter().take(ECHOED) {
            let _ = write!(line, " {byte:02x}");
        }
        if bytes.len() > ECHOED {
            let _ = write!(line, " ... (+{})", bytes.len() - ECHOED);
        }
        if self.text {
            if let Ok(text) = std::str::from_utf8(bytes) {
                let _ = write!(line, " {text:?}");
            }
        }
        writeln!(std::io::stdout(), "{line}")
    }

    fn lost(&mut self, count: u64) -> std::io::Result<()> {
        writeln!(std::io::stdout(), "lost {count}")
    }
}

fn kind_name(kind: TopicKind) -> &'static str {
    match kind {
        TopicKind::State => "state",
        _ => "event",
    }
}

struct Recorder {
    file: BufWriter<File>,
}

impl Recorder {
    fn create(path: &std::path::Path, subscriber: &Subscriber<AnyType>) -> std::io::Result<Self> {
        let mut file = BufWriter::new(File::create(path)?);
        let kind = match subscriber.topic_kind() {
            TopicKind::State => TOPIC_STATE,
            _ => TOPIC_EVENT,
        };
        let name = subscriber.topic_type().as_bytes();
        let name_len = u32::try_from(name.len()).map_err(std::io::Error::other)?;
        file.write_all(RECORD_MAGIC)?;
        file.write_all(&RECORD_VERSION.to_le_bytes())?;
        file.write_all(&(kind as u32).to_le_bytes())?;
        file.write_all(&name_len.to_le_bytes())?;
        file.write_all(name)?;
        file.flush()?;
        Ok(Self { file })
    }
}

impl Sink for Recorder {
    fn value(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        let len = u32::try_from(bytes.len())
            .ok()
            .filter(|&len| len != RECORD_LOST)
            .ok_or_else(|| std::io::Error::other("a record too long for the file"))?;
        self.file.write_all(&monotonic_ns().to_le_bytes())?;
        self.file.write_all(&len.to_le_bytes())?;
        self.file.write_all(bytes)?;
        self.file.flush()
    }

    fn lost(&mut self, count: u64) -> std::io::Result<()> {
        self.file.write_all(&monotonic_ns().to_le_bytes())?;
        self.file.write_all(&RECORD_LOST.to_le_bytes())?;
        self.file.write_all(&count.to_le_bytes())?;
        self.file.flush()
    }
}

/// `CLOCK_MONOTONIC` in nanoseconds.
fn monotonic_ns() -> u64 {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    // Seconds since boot and a nanosecond part below 10^9: neither is negative.
    (now.tv_sec as u64)
        .saturating_mul(1_000_000_000)
        .saturating_add(now.tv_nsec as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_lost_entry_cannot_be_taken_for_a_records_length() {
        assert_eq!(RECORD_LOST, 0xFFFF_FFFF);
        assert_eq!(RECORD_MAGIC, b"RSTR");
    }

    #[test]
    fn the_monotonic_clock_moves_forward() {
        let first = monotonic_ns();
        assert!(monotonic_ns() >= first);
    }
}

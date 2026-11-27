//! D-Bus's side, through `zbus`'s blocking API on the bus `DBUS_SESSION_BUS_ADDRESS` names
//! (`run.sh` starts a `dbus-broker` of its own): the producer process, which answers `Echo`
//! (B1) and emits the signal `Item` (B2, B3), a signal consumer and B1's client.
//!
//! A byte payload is an `ay` written and read as one block (`serde_bytes`), not an element at a
//! time. Signals have no flow control: with `--rate 0` the producer emits as fast as its socket
//! takes them, and a consumer the broker disconnects for its queue reports what it had.

use std::io::Write;
use std::os::fd::AsFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_bytes::{ByteBuf, Bytes};
use zbus::blocking::{connection, Connection, Proxy};
use zbus::proxy::CacheProperties;

use crate::common::*;

type Error = Box<dyn std::error::Error>;

const PATH: &str = "/org/rsbinder/Bench";
const INTERFACE: &str = "org.rsbinder.Bench";

fn ready(word: &str) {
    println!("{word}");
    std::io::stdout().flush().ok();
}

/// The bus name of the producer of `--name`.
fn bus_name(name: &str) -> String {
    format!("org.rsbinder.bench.{name}")
}

struct Producer {
    /// Set once the object is served: `Start` emits on it from a thread of its own.
    connection: Arc<Mutex<Option<Connection>>>,
    failures: Arc<AtomicI32>,
}

#[zbus::interface(name = "org.rsbinder.Bench")]
impl Producer {
    fn echo(&self, data: ByteBuf) -> ByteBuf {
        data
    }

    /// Emits `count` items of `size` bytes at `rate_hz` (0: as fast as the socket takes them),
    /// then `End`.
    fn start(&self, count: u32, size: u32, rate_hz: u32) {
        let Some(connection) = self.connection.lock().expect("lock").clone() else {
            return;
        };
        let failures = self.failures.clone();
        failures.store(0, Ordering::SeqCst);
        std::thread::spawn(move || {
            let mut item = vec![0u8; (size as usize).max(STAMP_BYTES)];
            let period = if rate_hz > 0 { 1_000_000_000 / u64::from(rate_hz) } else { 0 };
            let start = now_ns() + 1_000_000;
            let none = Option::<&str>::None;
            for seq in 0..u64::from(count) {
                if period > 0 {
                    sleep_until(start + seq * period);
                }
                stamp(&mut item, seq);
                let body = (Bytes::new(&item),);
                if connection.emit_signal(none, PATH, INTERFACE, "Item", &body).is_err() {
                    failures.fetch_add(1, Ordering::Relaxed);
                }
            }
            let _ = connection.emit_signal(none, PATH, INTERFACE, "End", &());
        });
    }

    fn failures(&self) -> i32 {
        self.failures.load(Ordering::SeqCst)
    }

    fn describe(&self, config: Config) -> Config {
        config
    }

    #[zbus(property)]
    fn value(&self) -> u64 {
        crate::rsb::VALUE
    }

    fn touch(&self, fd: zvariant::OwnedFd, len: u64) -> zbus::fdo::Result<u64> {
        touch(&fd, len as usize).map_err(|e| zbus::fdo::Error::Failed(e.to_string()))
    }
}

/// B8's argument, `(ixdbsasai(iid))`.
#[derive(Serialize, Deserialize, zvariant::Type, Clone, Debug)]
struct Config {
    id: i32,
    timestamp: i64,
    gain: f64,
    enabled: bool,
    name: String,
    tags: Vec<String>,
    samples: Vec<i32>,
    limits: Limits,
}

#[derive(Serialize, Deserialize, zvariant::Type, Clone, Debug)]
struct Limits {
    low: i32,
    high: i32,
    scale: f64,
}

fn config() -> Config {
    Config {
        id: 7,
        timestamp: now_ns() as i64,
        gain: 1.5,
        enabled: true,
        name: CONFIG_NAME.to_string(),
        tags: CONFIG_TAGS.iter().map(|t| t.to_string()).collect(),
        samples: (0..CONFIG_SAMPLES as i32).collect(),
        limits: Limits { low: -100, high: 100, scale: 0.25 },
    }
}

/// Child role `dbus-call`: `--count` calls of `--op`: `struct` (B8), `lookup` (B9: the bus
/// name's owner from the broker, and a proxy), `value` and `cached` (B10: `Properties.Get`,
/// and the proxy's property cache), `fd` (B11: a memfd of `--size` bytes sent as an `h`).
pub fn call_client(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 2) as usize);
    let name = a.str("name", "");
    let connection = Connection::session()?;
    let bench = proxy(&connection, &name)?;
    let record = match a.str("op", "").as_str() {
        "struct" => {
            let config = config();
            time_calls(a, |_| -> Result<(), Error> {
                let reply: Config = bench.call("Describe", &(&config,))?;
                if reply.samples.len() != CONFIG_SAMPLES || reply.limits.high != 100 {
                    return Err("another config came back".into());
                }
                Ok(())
            })?
        }
        "lookup" => {
            let bus = zbus::blocking::fdo::DBusProxy::new(&connection)?;
            let wanted = zbus::names::BusName::try_from(bus_name(&name))?;
            time_calls(a, |_| -> Result<(), Error> {
                bus.get_name_owner(wanted.clone())?;
                proxy(&connection, &name).map(drop)
            })?
        }
        "value" => time_calls(a, |_| -> Result<(), Error> {
            if bench.get_property::<u64>("Value")? != crate::rsb::VALUE {
                return Err("another value came back".into());
            }
            Ok(())
        })?,
        "cached" => {
            let cached = zbus::blocking::proxy::Builder::<Proxy<'_>>::new(&connection)
                .destination(bus_name(&name))?
                .path(PATH)?
                .interface(INTERFACE)?
                .cache_properties(CacheProperties::Yes)
                .build()?;
            // The first read fills the cache; the timed ones are answered from it.
            let _ = cached.get_property::<u64>("Value")?;
            time_calls(a, |_| -> Result<(), Error> {
                match cached.cached_property::<u64>("Value")? {
                    Some(crate::rsb::VALUE) => Ok(()),
                    other => Err(format!("cached value {other:?}").into()),
                }
            })?
        }
        "fd" => {
            let mut shared = SharedBytes::new(a.num("size", 4096) as usize)?;
            let len = shared.len() as u64;
            let fd = shared.fd.try_clone()?;
            time_calls(a, |seq| -> Result<(), Error> {
                let expected = shared.stamp(seq);
                let answer: u64 = bench.call("Touch", &(zvariant::Fd::from(fd.as_fd()), len))?;
                if answer != expected {
                    return Err("another stamp came back".into());
                }
                Ok(())
            })?
        }
        other => return Err(format!("unknown --op {other}").into()),
    };
    println!("{}", record.line());
    Ok(())
}

/// Child role `dbus-producer`: owns the bus name of `--name` until killed.
pub fn producer(a: &Args) -> Result<(), Error> {
    pin_server(a);
    let slot = Arc::new(Mutex::new(None));
    let object = Producer { connection: slot.clone(), failures: Arc::default() };
    let connection = connection::Builder::session()?
        .name(bus_name(&a.str("name", "")))?
        .serve_at(PATH, object)?
        .build()?;
    *slot.lock().expect("lock") = Some(connection);
    ready("READY");
    loop {
        std::thread::park();
    }
}

fn proxy<'c>(connection: &'c Connection, name: &str) -> Result<Proxy<'c>, Error> {
    Ok(zbus::blocking::proxy::Builder::new(connection)
        .destination(bus_name(name))?
        .path(PATH)?
        .interface(INTERFACE)?
        .cache_properties(CacheProperties::No)
        .build()?)
}

/// Child role `dbus-consumer`: one subscriber of `Item`.
pub fn consumer(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let connection = Connection::session()?;
    let proxy = proxy(&connection, &a.str("name", ""))?;
    let state = Arc::new((Mutex::new((Record::new(a.num("warmup", 0)), false)), Condvar::new()));
    for (signal, end) in [("Item", false), ("End", true)] {
        let state = state.clone();
        let mut signals = proxy.receive_signal(signal)?;
        std::thread::spawn(move || {
            let (record, arrived) = &*state;
            loop {
                let message = signals.next();
                let mut guard = record.lock().expect("lock");
                match message {
                    Some(message) if !end => match message.body().deserialize::<&[u8]>() {
                        Ok(data) => guard.0.take(data),
                        Err(e) => eprintln!("dbus-consumer: {e}"),
                    },
                    // The producer's last signal, or the bus closed this connection.
                    _ => guard.1 = true,
                }
                let done = guard.1;
                drop(guard);
                arrived.notify_all();
                if done {
                    return;
                }
            }
        });
    }
    ready("SUBSCRIBED");
    let (record, arrived) = &*state;
    let mut guard = record.lock().expect("lock");
    let mut seen = guard.0.received;
    while guard.0.received < count && !guard.1 {
        let (next, timeout) = arrived.wait_timeout(guard, SILENCE).expect("lock");
        guard = next;
        if timeout.timed_out() && guard.0.received == seen {
            break;
        }
        seen = guard.0.received;
    }
    if guard.0.received < count {
        guard.0.extra = format!("lost={}", count - guard.0.received);
    }
    println!("{}", guard.0.line());
    Ok(())
}

/// Child role `dbus-echo`: B1's client, round trips of `--size` bytes each way.
pub fn echo_client(a: &Args) -> Result<(), Error> {
    pin(a.num("cpu", 2) as usize);
    let count = a.num("count", 0);
    let warmup = a.num("warmup", 0);
    let connection = Connection::session()?;
    let proxy = proxy(&connection, &a.str("name", ""))?;
    let mut request = vec![0u8; a.num("size", 8) as usize];
    wait_for_start(a);
    let mut record = Record::new(0);
    for seq in 0..count {
        stamp(&mut request, seq);
        let start = now_ns();
        let reply: ByteBuf = proxy.call("Echo", &(Bytes::new(&request),))?;
        let end = now_ns();
        if reply.len() != request.len() {
            eprintln!("dbus-echo: reply of {} bytes", reply.len());
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

/// Driver side: starts the producer's run.
pub fn start(name: &str, count: u64, size: usize, rate: u64) -> Result<(), Error> {
    let connection = Connection::session()?;
    let args = (count as u32, size as u32, rate as u32);
    proxy(&connection, name)?.call::<_, _, ()>("Start", &args)?;
    // The reply is read; the broker may still hold what this connection queued.
    std::thread::sleep(Duration::from_millis(1));
    Ok(())
}

/// Driver side: how many emits the producer saw fail.
pub fn failures(name: &str) -> Result<i32, Error> {
    let connection = Connection::session()?;
    let failures = proxy(&connection, name)?.call("Failures", &())?;
    Ok(failures)
}

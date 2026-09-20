// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Streaming probe (Plan 10-7 Phase C, kernel half).
//!
//! `rsbinder::stream`'s unit tests put both ends in one process and
//! `tests/stream_rpc.rs` puts them on an RPC session. Neither runs a
//! batch through the kernel driver, which is where three things this
//! module depends on actually live:
//!
//! * `oneway` calls to one node stay in order, so the terminator cannot
//!   overtake the batches it follows (`binder.c`, `async_todo`);
//! * asynchronous transactions share half the receiving process's
//!   mapping, a limit the credit window has to stay under or the driver
//!   answers `FAILED_TRANSACTION` rather than blocking;
//! * a consumer whose process is gone turns the producer's next batch
//!   into `DeadObject` — and a producer parked for credit never gets
//!   that far, which is the case `link_to_death` on the sink covers.
//!
//! ```text
//! stream_probe serve   <name>
//! stream_probe consume <name> <count> <maxBatchBytes> <initialCredits>
//! stream_probe die     <name> <takeN> <maxBatchBytes> <initialCredits> <delayMicros>
//! stream_probe orphan  <name>
//! stream_probe status  <name>
//! ```
//!
//! `consume`, `die`, `orphan` and `status` each print one `RESULT` line;
//! `tests/scripts/run_stream_ac.sh` drives them.

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rsbinder::stream::{Receiver, Sink};
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/stream_demo.rs"));

use streamdemo::IStreamDemo::{BnStreamDemo, IStreamDemo};

#[derive(Default)]
struct DemoSvc {
    sent: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    last_error: Arc<AtomicI32>,
}

impl Interface for DemoSvc {}

impl IStreamDemo for DemoSvc {
    fn r#subscribe(
        &self,
        sink: &SIBinder,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
        delay_micros: i32,
    ) -> BinderResult<SIBinder> {
        let (mut producer, source) =
            Sink::<i32>::with_limits(sink, max_batch_bytes as usize, initial_credits as u32)?;
        let handle = source.as_binder();
        let sent = self.sent.clone();
        let finished = self.finished.clone();
        let last_error = self.last_error.clone();
        let delay = Duration::from_micros(delay_micros.max(0) as u64);
        // The handler returns the source at once; the pushing happens on
        // this thread, outside any transaction.
        thread::spawn(move || {
            let _source = source;
            let mut stopped_early = false;
            for item in 0..count {
                if let Err(e) = producer.send(&item) {
                    eprintln!("stream_probe: send failed at {item}: {e:?}");
                    last_error.store(i32::from(e), Ordering::SeqCst);
                    stopped_early = true;
                    break;
                }
                sent.fetch_add(1, Ordering::SeqCst);
                if !delay.is_zero() {
                    thread::sleep(delay);
                }
            }
            if !stopped_early {
                if let Err(e) = producer.end() {
                    eprintln!("stream_probe: end failed: {e:?}");
                    last_error.store(i32::from(e), Ordering::SeqCst);
                }
            }
            finished.store(true, Ordering::SeqCst);
        });
        Ok(handle)
    }

    fn r#subscribeFailing(
        &self,
        _sink: &SIBinder,
        _count: i32,
        _code: i32,
        _message: &str,
    ) -> BinderResult<SIBinder> {
        // The failing terminator is covered by the RPC half; this probe
        // exists for what only the driver shows.
        Err(Status::from(ExceptionCode::UnsupportedOperation))
    }

    fn r#sent(&self) -> BinderResult<i32> {
        Ok(self.sent.load(Ordering::SeqCst))
    }

    fn r#finished(&self) -> BinderResult<bool> {
        Ok(self.finished.load(Ordering::SeqCst))
    }

    fn r#lastError(&self) -> BinderResult<i32> {
        Ok(self.last_error.load(Ordering::SeqCst))
    }
}

fn serve(name: &str) -> Result<()> {
    let demo = BnStreamDemo::new_binder(DemoSvc::default());
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&demo))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn connect(name: &str) -> Result<Strong<dyn IStreamDemo>> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    if binder.as_remote().is_none() {
        eprintln!("stream_probe: {name} is local to this process, not a proxy");
        return Err(StatusCode::BadType);
    }
    <dyn IStreamDemo as FromIBinder>::try_from(binder)
}

/// Take the whole stream and report what arrived.
fn consume(name: &str, count: i32, max_batch_bytes: i32, initial_credits: i32) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, sink_binder) = Receiver::<i32>::new();
    let source = demo
        .r#subscribe(&sink_binder, count, max_batch_bytes, initial_credits, 0)
        .map_err(|e| e.transaction_error())?;
    rx.attach_source(&source)?;

    let mut received = 0i32;
    let mut ordered = true;
    let mut failure = None;
    for item in &mut rx {
        match item {
            // Order is the point: the driver may not reorder `oneway`
            // calls to one node, and a batch boundary must not drop or
            // duplicate an item either.
            Ok(item) => {
                if item != received {
                    ordered = false;
                }
                received += 1;
            }
            Err(status) => {
                failure = Some(format!("{status:?}"));
                break;
            }
        }
    }
    let ended = match failure {
        Some(detail) => format!("err:{detail}"),
        None => "ok".to_string(),
    };
    println!("RESULT consume {received} {ordered} {ended}");
    Ok(())
}

/// Subscribe, take `take` items, then leave without a word.
///
/// `std::process::exit` rather than a return: the point is a consumer
/// whose process is simply gone, so `Receiver::drop` must not get to send
/// its cancel first.
fn die(
    name: &str,
    take: i32,
    max_batch_bytes: i32,
    initial_credits: i32,
    delay_micros: i32,
) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, sink_binder) = Receiver::<i32>::new();
    let source = demo
        .r#subscribe(
            &sink_binder,
            i32::MAX,
            max_batch_bytes,
            initial_credits,
            delay_micros,
        )
        .map_err(|e| e.transaction_error())?;
    rx.attach_source(&source)?;

    let mut received = 0;
    while received < take {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Some(_)) => received += 1,
            Ok(None) => break,
            Err(status) => {
                println!("RESULT die ERROR {status:?}");
                return Ok(());
            }
        }
    }
    println!("RESULT die {received}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    std::process::exit(0);
}

/// Consume until the producer's process is killed out from under us.
///
/// The mirror of `die`: the producer paces itself and holds credit to
/// spare, so the consumer is blocked in `recv` with nothing in flight
/// when the service goes. Only the death link on the source can end that
/// wait.
fn orphan(name: &str) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, sink_binder) = Receiver::<i32>::new();
    let source = demo
        .r#subscribe(&sink_binder, i32::MAX, 4, 1_000_000, 5000)
        .map_err(|e| e.transaction_error())?;
    rx.attach_source(&source)?;

    let mut received = 0;
    let outcome = loop {
        match rx.recv_timeout(Duration::from_secs(20)) {
            Ok(Some(_)) => received += 1,
            // A clean terminator cannot happen here — the producer is
            // streaming `i32::MAX` items and is killed long before.
            Ok(None) if rx.is_finished() => break "ended-clean".to_string(),
            Ok(None) => break "timeout".to_string(),
            Err(status) => break format!("err:{:?}", status.transaction_error()),
        }
    };
    println!("RESULT orphan {received} {outcome}");
    Ok(())
}

fn status(name: &str) -> Result<()> {
    let demo = connect(name)?;
    let sent = demo.r#sent().map_err(|e| e.transaction_error())?;
    let finished = demo.r#finished().map_err(|e| e.transaction_error())?;
    let err = demo.r#lastError().map_err(|e| e.transaction_error())?;
    // Compared here rather than in the shell: the integer is a
    // discriminant of this build's `StatusCode`, not a wire constant.
    let dead = err == i32::from(StatusCode::DeadObject);
    println!("RESULT status sent={sent} finished={finished} dead={dead} err={err}");
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!(
            "usage: stream_probe serve <name>\n\
             \x20      stream_probe consume <name> <count> <maxBatchBytes> <initialCredits>\n\
             \x20      stream_probe die <name> <takeN> <maxBatchBytes> <initialCredits> <delayMicros>\n\
             \x20      stream_probe orphan <name>\n\
             \x20      stream_probe status <name>"
        );
        std::process::exit(2)
    };
    let num = |i: usize| -> i32 {
        args.get(i)
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| usage())
    };
    let r = match (args.get(1).map(String::as_str), args.len()) {
        (Some("serve"), 3) => serve(&args[2]),
        (Some("consume"), 6) => consume(&args[2], num(3), num(4), num(5)),
        (Some("die"), 7) => die(&args[2], num(3), num(4), num(5), num(6)),
        (Some("orphan"), 3) => orphan(&args[2]),
        (Some("status"), 3) => status(&args[2]),
        _ => usage(),
    };
    if let Err(e) = r {
        println!("RESULT ERROR {e:?}");
        eprintln!("stream_probe: {e:?}");
        std::process::exit(1);
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Streaming probe (Plan 10-7b, kernel half).
//!
//! `rsbinder::stream`'s unit tests put both ends in one process and
//! `tests/stream_rpc.rs` puts them on an RPC session. Neither carries the
//! endpoint through the kernel driver, which is where three things this
//! module depends on actually live:
//!
//! * the ring's memfd crosses as a file descriptor inside the
//!   `MQDescriptor` — in an argument for a download, in a reply for an
//!   upload — and the producer maps memory the consumer allocated and
//!   sealed in another process;
//! * the two processes wake each other through the ring's futex word,
//!   with no binder call carrying an item — so a long stream is paced by
//!   the ring alone and never touches the receiver's asynchronous space;
//! * a peer whose process is gone is seen only through a death link: a
//!   producer parked on a full ring and a consumer parked on an empty
//!   one have nothing in flight to fail.
//!
//! ```text
//! stream_probe serve   <name>
//! stream_probe consume <name> <count> <ringBytes>
//! stream_probe pause   <name> <count> <ringBytes> <takeN>
//! stream_probe crowd   <name> <streams> <count>
//! stream_probe die     <name> <takeN> <ringBytes> <delayMicros>
//! stream_probe cancel  <name> <takeN> <ringBytes>
//! stream_probe orphan  <name>
//! stream_probe upload  <name> <count> <ringBytes>
//! stream_probe vanish  <name> <sendN>
//! stream_probe status  <name>
//! stream_probe serve-rpc <socketPath>          (feature `rpc`)
//! ```
//!
//! Every mode but `serve` and `serve-rpc` prints one `RESULT` line;
//! `tests/scripts/run_stream_ac.sh` drives them. The `subscribe` call's
//! batch and credit arguments only shape the RPC path, so the probe
//! passes the producer's defaults and lets the ring do the pacing.
//!
//! `serve-rpc` is the same service as the root of an `RpcServer` on a
//! Unix socket, for a libbinder `RpcSession` client to stream against
//! (`example-hello/cpp/run_stream_rpc_interop.sh`).

use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use rsbinder::stream::{
    Receiver, ReceiverPolicy, Sink, SinkPolicy, StreamEndpoint, Token, END_RESERVE,
};
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/stream_demo.rs"));

use streamdemo::IStreamDemo::{BnStreamDemo, IStreamDemo};

#[derive(Default)]
struct DemoSvc {
    sent: Arc<AtomicI32>,
    finished: Arc<AtomicBool>,
    last_error: Arc<AtomicI32>,
    uploaded: Arc<AtomicI32>,
    upload_ordered: Arc<AtomicBool>,
    upload_finished: Arc<AtomicBool>,
    upload_error: Arc<AtomicI32>,
}

impl Interface for DemoSvc {}

impl IStreamDemo for DemoSvc {
    fn r#subscribe(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        max_batch_bytes: i32,
        initial_credits: i32,
        delay_micros: i32,
    ) -> BinderResult<()> {
        let policy = SinkPolicy {
            max_batch_bytes: max_batch_bytes as usize,
            initial_credits: initial_credits as u32,
            ..SinkPolicy::default()
        };
        let mut producer = Sink::<i32>::open_with(endpoint, &policy)?;
        // Per stream, so a client polling `finished` does not see the previous stream's.
        self.finished.store(false, Ordering::SeqCst);
        self.last_error.store(0, Ordering::SeqCst);
        let sent = self.sent.clone();
        let finished = self.finished.clone();
        let last_error = self.last_error.clone();
        let delay = Duration::from_micros(delay_micros.max(0) as u64);
        // The handler returns at once; the pushing happens on this
        // thread, outside any transaction, and blocks there whenever the
        // ring is full.
        thread::spawn(move || {
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
        Ok(())
    }

    // The async producer is the RPC half's; this probe is for what only the driver shows.
    fn r#subscribeAsync(
        &self,
        _endpoint: &StreamEndpoint,
        _count: i32,
        _max_batch_bytes: i32,
        _initial_credits: i32,
    ) -> BinderResult<()> {
        Err(Status::from(ExceptionCode::UnsupportedOperation))
    }

    /// `count` items, then the failure: the end record `run_stream_interop.sh` parses in C.
    fn r#subscribeFailing(
        &self,
        endpoint: &StreamEndpoint,
        count: i32,
        code: i32,
        message: &str,
    ) -> BinderResult<()> {
        let mut producer = Sink::<i32>::open(endpoint)?;
        let status = Status::new_service_specific_error(code, Some(message.to_string()));
        thread::spawn(move || {
            let sent = (0..count).try_for_each(|item| producer.send(&item));
            let ended = match sent {
                Ok(()) => producer.end_with(&status),
                Err(e) => Err(e),
            };
            if let Err(e) = ended {
                eprintln!("stream_probe: subscribeFailing: {e:?}");
            }
        });
        Ok(())
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

    /// The service consumes: the receiver is made in the handler and read on its own thread.
    fn r#upload(&self, producer: &SIBinder, ring_bytes: i32) -> BinderResult<StreamEndpoint> {
        let (mut rx, endpoint) = Receiver::<i32>::with_policy(
            producer,
            &ReceiverPolicy {
                ring_bytes: ring_bytes.max(0) as usize,
                ..ReceiverPolicy::default()
            },
        )?;
        let uploaded = self.uploaded.clone();
        let ordered = self.upload_ordered.clone();
        let finished = self.upload_finished.clone();
        let error = self.upload_error.clone();
        ordered.store(true, Ordering::SeqCst);
        finished.store(false, Ordering::SeqCst);
        error.store(0, Ordering::SeqCst);
        thread::spawn(move || {
            let mut expected = 0i32;
            while let Some(item) = rx.recv() {
                match item {
                    Ok(item) => {
                        if item != expected {
                            ordered.store(false, Ordering::SeqCst);
                        }
                        expected += 1;
                        uploaded.fetch_add(1, Ordering::SeqCst);
                    }
                    Err(status) => {
                        eprintln!("stream_probe: upload ended with {status:?}");
                        error.store(i32::from(status.transaction_error()), Ordering::SeqCst);
                        break;
                    }
                }
            }
            finished.store(true, Ordering::SeqCst);
        });
        Ok(endpoint)
    }

    fn r#uploaded(&self) -> BinderResult<i32> {
        Ok(self.uploaded.load(Ordering::SeqCst))
    }

    fn r#uploadOrdered(&self) -> BinderResult<bool> {
        Ok(self.upload_ordered.load(Ordering::SeqCst))
    }

    fn r#uploadFinished(&self) -> BinderResult<bool> {
        Ok(self.upload_finished.load(Ordering::SeqCst))
    }

    fn r#uploadError(&self) -> BinderResult<i32> {
        Ok(self.upload_error.load(Ordering::SeqCst))
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

/// The service as an RPC root: a libbinder client asks for the root object, not a name.
#[cfg(feature = "rpc")]
fn serve_rpc(path: &str) -> Result<()> {
    let server = rsbinder::rpc::RpcServer::setup_unix_server(path)?;
    server.set_android13plus(2);
    // Batches, grants and cancels are oneway calls from the client; a few threads take them.
    server.set_max_threads(4);
    server.set_root(BnStreamDemo::new_binder(DemoSvc::default()).as_binder())?;
    println!("SERVING {path}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn connect_via(uri: &str, name: &str) -> Result<Strong<dyn IStreamDemo>> {
    let binder = rsbinder::Client::open(uri)
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    if binder.as_remote().is_none() {
        eprintln!("stream_probe: {name} is local to this process, not a proxy");
        return Err(StatusCode::BadType);
    }
    <dyn IStreamDemo as FromIBinder>::try_from(binder)
}

fn connect(name: &str) -> Result<Strong<dyn IStreamDemo>> {
    connect_via("binder://", name)
}

/// A receiver against the service, its endpoint carrying a ring of
/// `ring_bytes`: the service is a kernel proxy here.
fn receiver_for(
    demo: &Strong<dyn IStreamDemo>,
    ring_bytes: usize,
) -> Result<(Receiver<i32>, StreamEndpoint)> {
    Receiver::with_policy(
        &demo.as_binder(),
        &ReceiverPolicy {
            ring_bytes,
            ..ReceiverPolicy::default()
        },
    )
}

/// `subscribe` with the RPC path's arguments at the producer's defaults;
/// on a ring they are not read.
fn subscribe(
    demo: &Strong<dyn IStreamDemo>,
    endpoint: &StreamEndpoint,
    count: i32,
    delay_micros: i32,
) -> Result<()> {
    let defaults = SinkPolicy::default();
    demo.r#subscribe(
        endpoint,
        count,
        defaults.max_batch_bytes as i32,
        defaults.initial_credits as i32,
        delay_micros,
    )
    .map_err(|e| e.transaction_error())
}

/// Items of an `i32` stream that fit the part of a ring items may use:
/// each is a 4-byte header and a 4-byte payload.
fn ring_items(ring_bytes: usize) -> i32 {
    ((ring_bytes - END_RESERVE) / 8) as i32
}

/// Drain `rx` to the end and say what came: items, whether they were
/// `from..` in order, and how the stream ended.
fn drain(rx: &mut Receiver<i32>, from: i32) -> (i32, bool, String) {
    let mut received = 0i32;
    let mut expected = from;
    let mut ordered = true;
    let mut failure = None;
    for item in rx {
        match item {
            // Order is the point: records leave the ring in the order
            // they were committed, and neither end may drop or duplicate
            // one at a wrap.
            Ok(item) => {
                if item != expected {
                    ordered = false;
                }
                expected += 1;
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
    (received, ordered, ended)
}

/// Take the whole stream and report what arrived.
fn consume(name: &str, count: i32, ring_bytes: usize) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, endpoint) = receiver_for(&demo, ring_bytes)?;
    subscribe(&demo, &endpoint, count, 0)?;
    let (received, ordered, ended) = drain(&mut rx, 0);
    println!("RESULT consume {received} {ordered} {ended}");
    Ok(())
}

/// Take `take` items, stop reading, and watch the producer stop at the
/// ring's capacity; then read the rest.
fn pause(name: &str, count: i32, ring_bytes: usize, take: i32) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, endpoint) = receiver_for(&demo, ring_bytes)?;
    subscribe(&demo, &endpoint, count, 0)?;

    let mut taken = 0i32;
    let mut ordered = true;
    while taken < take {
        match rx.recv() {
            Some(Ok(item)) => {
                if item != taken {
                    ordered = false;
                }
                taken += 1;
            }
            Some(Err(status)) => {
                println!("RESULT pause ERROR {status:?}");
                return Ok(());
            }
            None => {
                println!("RESULT pause ERROR ended-early");
                return Ok(());
            }
        }
    }

    let full = take + ring_items(ring_bytes);
    let mut sent;
    loop {
        sent = demo.r#sent().map_err(|e| e.transaction_error())?;
        if sent >= full {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    // Read once more: the counter rose to `full` and must stay there —
    // an overshoot would mean the ring admitted more than it holds.
    let again = demo.r#sent().map_err(|e| e.transaction_error())?;
    let finished = demo.r#finished().map_err(|e| e.transaction_error())?;
    let parked = sent == full && again == full && !finished;
    if !parked {
        eprintln!("stream_probe: sent={sent} again={again} finished={finished} full={full}");
    }

    let (rest, rest_ordered, ended) = drain(&mut rx, take);
    let received = taken + rest;
    let ordered = ordered && rest_ordered;
    println!("RESULT pause {received} {ordered} parked={parked} {ended}");
    Ok(())
}

/// `streams` streams from one service at once, into a process whose
/// binder mapping is a single page.
fn crowd(name: &str, streams: usize, count: i32) -> Result<()> {
    let page = rustix::param::page_size();
    let demo = connect_via(&format!("binder://?mmap={page}"), name)?;
    let mut receivers = Vec::with_capacity(streams);
    for _ in 0..streams {
        let (rx, endpoint) = Receiver::<i32>::new(&demo.as_binder())?;
        subscribe(&demo, &endpoint, count, 0)?;
        receivers.push(rx);
    }
    let workers: Vec<_> = receivers
        .into_iter()
        .map(|mut rx| thread::spawn(move || drain(&mut rx, 0)))
        .collect();
    let mut total = 0i32;
    let mut ordered = true;
    let mut ended = "ok".to_string();
    for worker in workers {
        let (received, in_order, end) = worker.join().expect("a consumer thread");
        total += received;
        ordered &= in_order;
        if end != "ok" && ended == "ok" {
            ended = end;
        }
    }
    println!("RESULT crowd {streams} {total} {ordered} {ended}");
    Ok(())
}

/// Take `take` items, then exit without `Receiver::drop`, so `CANCEL` is never set.
fn die(name: &str, take: i32, ring_bytes: usize, delay_micros: i32) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, endpoint) = receiver_for(&demo, ring_bytes)?;
    subscribe(&demo, &endpoint, i32::MAX, delay_micros)?;

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

/// Take `take` items and drop the receiver: a return, since `Receiver::drop` is what cancels.
fn cancel(name: &str, take: i32, ring_bytes: usize) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, endpoint) = receiver_for(&demo, ring_bytes)?;
    subscribe(&demo, &endpoint, i32::MAX, 0)?;

    let mut received = 0;
    while received < take {
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Some(_)) => received += 1,
            Ok(None) => break,
            Err(status) => {
                println!("RESULT cancel ERROR {status:?}");
                return Ok(());
            }
        }
    }
    drop(rx);
    println!("RESULT cancel {received}");
    Ok(())
}

/// Consume until the producer's process is killed out from under us.
fn orphan(name: &str) -> Result<()> {
    let demo = connect(name)?;
    let (mut rx, endpoint) = receiver_for(&demo, ReceiverPolicy::default().ring_bytes)?;
    subscribe(&demo, &endpoint, i32::MAX, 5000)?;

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

/// Push `count` items and report what the service took; the `Token` lives until both sides end.
fn upload(name: &str, count: i32, ring_bytes: usize) -> Result<()> {
    let demo = connect(name)?;
    let token = Token::new();
    let endpoint = demo
        .r#upload(&token.binder(), ring_bytes as i32)
        .map_err(|e| e.transaction_error())?;
    let mut tx = Sink::<i32>::open(&endpoint)?;
    for item in 0..count {
        tx.send(&item)?;
    }
    tx.end()?;
    // The end record is in the ring; the service's thread has yet to
    // read it. Polled on the service's own flag, bounded by the script's
    // `timeout`.
    while !demo.r#uploadFinished().map_err(|e| e.transaction_error())? {
        thread::sleep(Duration::from_millis(20));
    }
    let uploaded = demo.r#uploaded().map_err(|e| e.transaction_error())?;
    let ordered = demo.r#uploadOrdered().map_err(|e| e.transaction_error())?;
    let err = demo.r#uploadError().map_err(|e| e.transaction_error())?;
    let ended = if err == 0 {
        "ok".to_string()
    } else {
        format!("err:{err}")
    };
    println!("RESULT upload {uploaded} {ordered} {ended}");
    drop(token);
    Ok(())
}

/// Ask for an upload endpoint, push `send` items into it, and leave
/// without ending the stream.
fn vanish(name: &str, send: i32) -> Result<()> {
    let demo = connect(name)?;
    let token = Token::new();
    let endpoint = demo
        .r#upload(&token.binder(), ReceiverPolicy::default().ring_bytes as i32)
        .map_err(|e| e.transaction_error())?;
    let mut sent = 0;
    if send > 0 {
        let mut tx = Sink::<i32>::open(&endpoint)?;
        for item in 0..send {
            tx.send(&item)?;
            sent += 1;
        }
        std::mem::forget(tx);
    }
    println!("RESULT vanish {sent}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    // Neither the sink's end record nor the token's unlink: the process
    // is simply gone.
    std::process::exit(0);
}

fn status(name: &str) -> Result<()> {
    let demo = connect(name)?;
    let code = |e: Status| e.transaction_error();
    let sent = demo.r#sent().map_err(code)?;
    let finished = demo.r#finished().map_err(code)?;
    let err = demo.r#lastError().map_err(code)?;
    let uploaded = demo.r#uploaded().map_err(code)?;
    let up_ordered = demo.r#uploadOrdered().map_err(code)?;
    let up_finished = demo.r#uploadFinished().map_err(code)?;
    let up_err = demo.r#uploadError().map_err(code)?;
    // Compared here rather than in the shell: the integer is a
    // discriminant of this build's `StatusCode`, not a wire constant.
    let dead_code = i32::from(StatusCode::DeadObject);
    let dead = err == dead_code;
    let canceled = err == i32::from(StatusCode::InvalidOperation);
    let up_dead = up_err == dead_code;
    println!(
        "RESULT status sent={sent} finished={finished} dead={dead} canceled={canceled} err={err} \
         up_received={uploaded} up_ordered={up_ordered} up_finished={up_finished} \
         up_dead={up_dead} up_err={up_err}"
    );
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!(
            "usage: stream_probe serve <name>\n\
             \x20      stream_probe consume <name> <count> <ringBytes>\n\
             \x20      stream_probe pause <name> <count> <ringBytes> <takeN>\n\
             \x20      stream_probe crowd <name> <streams> <count>\n\
             \x20      stream_probe die <name> <takeN> <ringBytes> <delayMicros>\n\
             \x20      stream_probe cancel <name> <takeN> <ringBytes>\n\
             \x20      stream_probe orphan <name>\n\
             \x20      stream_probe upload <name> <count> <ringBytes>\n\
             \x20      stream_probe vanish <name> <sendN>\n\
             \x20      stream_probe status <name>\n\
             \x20      stream_probe serve-rpc <socketPath>"
        );
        std::process::exit(2)
    };
    let num = |i: usize| -> i32 {
        args.get(i)
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| usage())
    };
    let size = |i: usize| -> usize {
        args.get(i)
            .and_then(|a| a.parse().ok())
            .unwrap_or_else(|| usage())
    };
    let r = match (args.get(1).map(String::as_str), args.len()) {
        (Some("serve"), 3) => serve(&args[2]),
        (Some("consume"), 5) => consume(&args[2], num(3), size(4)),
        (Some("pause"), 6) => pause(&args[2], num(3), size(4), num(5)),
        (Some("crowd"), 5) => crowd(&args[2], size(3), num(4)),
        (Some("die"), 6) => die(&args[2], num(3), size(4), num(5)),
        (Some("cancel"), 5) => cancel(&args[2], num(3), size(4)),
        (Some("orphan"), 3) => orphan(&args[2]),
        (Some("upload"), 5) => upload(&args[2], num(3), size(4)),
        (Some("vanish"), 4) => vanish(&args[2], num(3)),
        (Some("status"), 3) => status(&args[2]),
        #[cfg(feature = "rpc")]
        (Some("serve-rpc"), 3) => serve_rpc(&args[2]),
        _ => usage(),
    };
    if let Err(e) = r {
        println!("RESULT ERROR {e:?}");
        eprintln!("stream_probe: {e:?}");
        std::process::exit(1);
    }
}

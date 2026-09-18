// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Cancellation probe (Plan 10-5 AC-5.1 kernel half, AC-5.4 STAGE3).
//!
//! A cancellation transport is only interesting once it has crossed an
//! address space: it leaves as a binder object in a reply and comes back
//! as a proxy whose `oneway cancel()` has to reach the original signal.
//! On kernel binder that takes two processes — a binder resolved inside
//! the process that owns it is dispatched in-process and the driver's
//! object marshalling is never exercised.
//!
//! ```text
//! cancel_probe serve  <service-name>     # hands out transports, then blocks
//! cancel_probe cancel <service-name>     # starts a job, cancels it, reports
//! ```
//!
//! `cancel` prints one line:
//!
//! ```text
//! RESULT cancel <BEFORE:false|true> <AFTER:false|true>
//! RESULT cancel ERROR <detail>
//! ```
//!
//! The wire contract is shared with
//! `example-hello/cpp/cancel_interop.cpp`, which plays either role
//! against this one; `run_cancel_ac.sh` (local kernel) and
//! `run_cancel_interop.sh` (emulator) drive them.

use std::sync::Mutex;

use rsbinder::cancel::CancellationSignal;
use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.cancel.IProbe";
/// Reply: the transport binder for a fresh signal.
const START: TransactionCode = FIRST_CALL_TRANSACTION;
/// Reply: `1` if the signal from the last `START` has been cancelled.
const STATUS: TransactionCode = FIRST_CALL_TRANSACTION + 1;

struct Probe {
    /// The signal handed out by the last `START`. One is enough for a
    /// probe; a real service keys them by operation.
    current: Mutex<Option<CancellationSignal>>,
}

impl Interface for Probe {}

impl Remotable for Probe {
    fn descriptor() -> &'static str {
        DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            START => {
                let signal = CancellationSignal::new();
                let transport = signal.create_transport();
                *self.current.lock().unwrap() = Some(signal);
                eprintln!("cancel_probe: handed out a transport");
                reply.write(&transport.as_binder())
            }
            STATUS => {
                let canceled = self
                    .current
                    .lock()
                    .unwrap()
                    .as_ref()
                    .map(|s| s.is_canceled())
                    .unwrap_or(false);
                eprintln!("cancel_probe: status canceled={canceled}");
                reply.write(&(canceled as i32))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn serve(name: &str) -> Result<()> {
    let probe = Binder::new(Probe {
        current: Mutex::new(None),
    });
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&probe))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn status(remote: &dyn RemoteProxy) -> Result<bool> {
    let data = remote.prepare_transact(true)?;
    let mut reply = remote
        .submit_transact(STATUS, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);
    let canceled: i32 = reply.read()?;
    Ok(canceled != 0)
}

fn cancel(name: &str) -> Result<()> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let remote = binder.as_remote().ok_or_else(|| {
        eprintln!("cancel_probe: {name} is local to this process, not a proxy");
        StatusCode::BadType
    })?;

    let data = remote.prepare_transact(true)?;
    let mut reply = remote
        .submit_transact(START, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);
    let transport: SIBinder = reply.read()?;

    let before = status(remote)?;
    rsbinder::cancel::cancel_remote(&transport)?;
    // Poll rather than read once. `cancel` is oneway *and* goes to the
    // transport's node, while `STATUS` goes to the service's: the kernel
    // orders neither against the other, so a single read here reports
    // `false` whenever the status call is served first (measured — it is
    // not rare). Delivery is what is under test, not its latency.
    let mut after = false;
    for _ in 0..50 {
        after = status(remote)?;
        if after {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    println!("RESULT cancel {before} {after}");
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: cancel_probe serve <name> | cancel <name>");
        std::process::exit(2)
    };
    if args.len() != 3 {
        usage();
    }
    let r = match args[1].as_str() {
        "serve" => serve(&args[2]),
        "cancel" => cancel(&args[2]),
        _ => usage(),
    };
    if let Err(e) = r {
        println!("RESULT cancel ERROR {e:?}");
        eprintln!("cancel_probe: {e:?}");
        std::process::exit(1);
    }
}

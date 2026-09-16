// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Receive-mapping size probe (Plan 10-1 AC-1.4).
//!
//! The "1 MB binder limit" is the size of the mapping the **receiving**
//! process made, so it takes two processes to observe it: a service that
//! maps a chosen size and a caller that sends a payload against it. One
//! process cannot stand in for both — a binder resolved inside the
//! process that owns it is dispatched in-process, and the driver (the
//! only thing that enforces the size) is never reached.
//!
//! ```text
//! mmap_probe serve <bytes|default> <service-name>   # registers, then blocks
//! mmap_probe call  <bytes> <service-name>           # one transaction, prints RESULT
//! ```
//!
//! `call` prints exactly one line:
//!
//! ```text
//! RESULT call <bytes> OK           # the service received and echoed the length
//! RESULT call <bytes> FAILEDTXN    # the driver refused it: no room in the receiver
//! RESULT call <bytes> ERROR <code> # anything else
//! ```
//!
//! Driven by `tests/scripts/run_mmap_size_ac.sh`, which needs a live
//! `rsb_hub`.

use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.mmap.IProbe";
/// Takes a `byte[]`, returns its length as an `int`. Hand-written on
/// both sides: the point of the test is the payload size, and an
/// interface definition would add nothing to it.
const CONSUME: TransactionCode = FIRST_CALL_TRANSACTION;

struct Probe;

impl Interface for Probe {}

impl Remotable for Probe {
    fn descriptor() -> &'static str {
        DESCRIPTOR
    }

    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            CONSUME => {
                let blob: Vec<u8> = reader.read()?;
                eprintln!("mmap_probe: received {} bytes", blob.len());
                reply.write(&(blob.len() as i32))
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn serve(size: &str, name: &str) -> Result<()> {
    let uri = match size {
        "default" => "binder://".to_string(),
        n => format!("binder://?mmap={n}"),
    };
    let probe = Binder::new(Probe);
    let server = rsbinder::serve(&uri)?.add(name, Interface::as_binder(&probe))?;
    println!(
        "SERVING {name} mmap={}",
        ProcessState::as_self().mmap_size()
    );
    // stdout is a pipe under the driving script, so the line above has
    // to be flushed before this blocks forever.
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn call(bytes: usize, name: &str) -> Result<()> {
    // A plain client: it neither serves nor receives callbacks, so the
    // default mapping is all it needs (the reply here is four bytes).
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let remote = binder.as_remote().ok_or_else(|| {
        // In-process would prove nothing — see the module doc.
        eprintln!("mmap_probe: {name} is local to this process, not a proxy");
        StatusCode::BadType
    })?;

    let payload = vec![0xa5u8; bytes];
    let mut data = remote.prepare_transact(true)?;
    data.write(&payload)?;

    match remote.submit_transact(CONSUME, &data, 0) {
        Ok(Some(mut reply)) => {
            reply.set_data_position(0);
            let echoed: i32 = reply.read()?;
            if echoed as usize == bytes {
                println!("RESULT call {bytes} OK");
            } else {
                println!("RESULT call {bytes} ERROR echoed={echoed}");
            }
        }
        Ok(None) => println!("RESULT call {bytes} ERROR no-reply"),
        Err(StatusCode::FailedTransaction) => println!("RESULT call {bytes} FAILEDTXN"),
        Err(e) => println!("RESULT call {bytes} ERROR {e:?}"),
    }
    Ok(())
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: mmap_probe serve <bytes|default> <name> | call <bytes> <name>");
        std::process::exit(2)
    };
    if args.len() != 4 {
        usage();
    }
    let r = match args[1].as_str() {
        "serve" => serve(&args[2], &args[3]),
        "call" => match args[2].parse() {
            Ok(n) => call(n, &args[3]),
            Err(_) => usage(),
        },
        _ => usage(),
    };
    if let Err(e) = r {
        eprintln!("mmap_probe: {e:?}");
        std::process::exit(1);
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Typed service-specific error probe (Plan 10-4 AC-4.5).
//!
//! The rsbinder half of the STAGE3 harness: one role throws a typed
//! error for a real `libbinder` client to read, the other reads one a
//! real `libbinder` service threw. The C++ half is
//! `example-hello/cpp/typed_error_interop.cpp`; both are driven by
//! `run_typed_error_interop.sh`.
//!
//! ```text
//! typed_error_probe serve <service-name>          # throws, then blocks
//! typed_error_probe call  <service-name> <code>   # reads what the peer threw
//! ```
//!
//! `call` prints one line:
//!
//! ```text
//! RESULT typed <code> <TYPED name|UNTYPED code|ERROR detail>
//! ```
//!
//! Exit 0 on a service-specific answer (`TYPED` or `UNTYPED`), 1
//! otherwise — the convention the C++ half already follows.
//!
//! Hand-written on both sides rather than generated from `.aidl`,
//! because what is under test is the `Status` header — the one part of a
//! reply the generated code would otherwise hide.

use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.typederr.IProbe";
/// Takes an `int` and always fails with it as a service-specific error.
const THROW: TransactionCode = FIRST_CALL_TRANSACTION;

declare_binder_enum! {
    /// The codes both halves agree on. Declared here rather than in
    /// `.aidl` for the same reason as everywhere else: which enum a
    /// service's error codes belong to is not something `.aidl` says.
    ProbeError : [i32; 3] {
        REFUSED = 1,
        EXHAUSTED = 2,
        LOCKED = 3,
    }
}
impl_service_specific_error!(ProbeError);

fn name_of(err: ProbeError) -> &'static str {
    match err {
        ProbeError::REFUSED => "REFUSED",
        ProbeError::EXHAUSTED => "EXHAUSTED",
        ProbeError::LOCKED => "LOCKED",
        _ => "UNNAMED",
    }
}

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
            THROW => {
                let requested: i32 = reader.read()?;
                // Built from the type when the code is one of ours, so a
                // real libbinder client is reading what
                // `Status::service_specific` wrote.
                let status = match ProbeError::from_code(requested) {
                    Some(err) => Status::service_specific(err, Some("typed from rsbinder")),
                    None => {
                        Status::new_service_specific_error(requested, Some("untyped".to_owned()))
                    }
                };
                eprintln!("typed_error_probe: throwing {requested}");
                // A failing AIDL stub writes the status and nothing
                // else; the transaction itself succeeds.
                reply.write(&status)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn serve(name: &str) -> Result<()> {
    let probe = Binder::new(Probe);
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&probe))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

/// `Ok(false)` = the peer answered with something other than a
/// service-specific error; the caller turns that into a non-zero exit,
/// as the C++ half does.
fn call(name: &str, code: i32) -> Result<bool> {
    let binder = rsbinder::Client::open("binder://")
        .and_then(|_| hub::check_service(name).ok_or(StatusCode::NameNotFound))?;
    let remote = binder.as_remote().ok_or_else(|| {
        eprintln!("typed_error_probe: {name} is local to this process, not a proxy");
        StatusCode::BadType
    })?;

    let mut data = remote.prepare_transact(true)?;
    data.write(&code)?;
    let mut reply = match remote.submit_transact(THROW, &data, 0)? {
        Some(reply) => reply,
        None => {
            println!("RESULT typed {code} ERROR no-reply");
            return Ok(false);
        }
    };
    reply.set_data_position(0);
    let status: Status = reply.read()?;

    if status.exception_code() != ExceptionCode::ServiceSpecific {
        println!(
            "RESULT typed {code} ERROR exception={:?}",
            status.exception_code()
        );
        return Ok(false);
    }
    match status.service_error::<ProbeError>() {
        // The typed read is the claim: the peer's `i32` came back as one
        // of this process's own values.
        Some(err) => println!("RESULT typed {code} TYPED {}", name_of(err)),
        // Still a service-specific failure, just not a code this build
        // declares — what a newer peer looks like from here.
        None => println!(
            "RESULT typed {code} UNTYPED {}",
            status.service_specific_error()
        ),
    }
    eprintln!("typed_error_probe: message={:?}", status.message());
    Ok(true)
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!("usage: typed_error_probe serve <name> | call <name> <code>");
        std::process::exit(2)
    };
    match args.get(1).map(String::as_str) {
        Some("serve") if args.len() == 3 => {
            if let Err(e) = serve(&args[2]) {
                eprintln!("typed_error_probe: {e:?}");
                std::process::exit(1);
            }
        }
        Some("call") if args.len() == 4 => {
            let code = match args[3].parse() {
                Ok(code) => code,
                Err(_) => usage(),
            };
            match call(&args[2], code) {
                Ok(true) => {}
                Ok(false) => std::process::exit(1),
                Err(e) => {
                    // The driving script discards stderr and judges the
                    // stdout line, so a failed lookup has to say so there.
                    println!("RESULT typed {code} ERROR {e:?}");
                    eprintln!("typed_error_probe: {e:?}");
                    std::process::exit(1);
                }
            }
        }
        _ => usage(),
    }
}

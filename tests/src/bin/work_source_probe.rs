// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Work source probe (Plan 10-9 AC-9.1/9.2 kernel half).
//!
//! The work source rides in the kernel request header, so what is under
//! test is one process setting it, the driver carrying it, and another
//! process reading it — plus the rule that a received value goes no
//! further unless the handler sets it again, which takes a third hop.
//!
//! ```text
//! work_source_probe serve <name> [<next>]   # serves; FORWARD calls <next>
//! work_source_probe check <front>           # runs the checks against <front>
//! work_source_probe get <name> <uid|unset>  # one GET, after setting <uid>
//! ```
//!
//! `check` prints `PASS`/`FAIL` lines and ends with `RESULT ok` or
//! `RESULT fail`. `tests/scripts/run_work_source_ac.sh` drives it.
//!
//! `get` prints `RESULT rs get <uid|unset> SEEN <uid> <propagate>`. It and
//! `serve` are the rsbinder end of the AC-9.3 STAGE3 harness, whose AOSP
//! end (`example-hello/cpp/work_source_interop.cpp`) speaks the same
//! protocol; `example-hello/cpp/run_work_source_interop.sh` drives both.

use rsbinder::*;

const DESCRIPTOR: &str = "rsbinder.test.worksource.IProbe";
/// Reply: `[uid][propagate]` as the handler sees them.
const GET: TransactionCode = FIRST_CALL_TRANSACTION;
/// Arg: `i32` uid to set before forwarding, or [`NO_SET`]. Calls `GET` on
/// the next service. Reply: `[uid received][uid the next service saw]`.
const FORWARD: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const NO_SET: i32 = -2;
const UNSET: i32 = -1;

struct Probe {
    next: Option<String>,
}

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
            GET => {
                reply.write(&(get_calling_work_source_uid() as i32))?;
                reply.write(&(should_propagate_work_source() as i32))
            }
            FORWARD => {
                let received = get_calling_work_source_uid() as i32;
                let set: i32 = reader.read()?;
                if set != NO_SET {
                    set_calling_work_source_uid(set as _);
                }
                let next = self.next.as_deref().ok_or(StatusCode::InvalidOperation)?;
                let (seen, _) = get(&lookup(next)?)?;
                reply.write(&received)?;
                reply.write(&seen)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }

    fn on_dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

fn lookup(name: &str) -> Result<SIBinder> {
    hub::check_service(name).ok_or(StatusCode::NameNotFound)
}

fn remote(binder: &SIBinder) -> Result<&dyn RemoteProxy> {
    binder.as_remote().ok_or(StatusCode::BadType)
}

fn get(binder: &SIBinder) -> Result<(i32, bool)> {
    let remote = remote(binder)?;
    let data = remote.prepare_transact(true)?;
    let mut reply = remote
        .submit_transact(GET, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);
    let uid: i32 = reply.read()?;
    let propagate: i32 = reply.read()?;
    Ok((uid, propagate != 0))
}

fn forward(binder: &SIBinder, set: i32) -> Result<(i32, i32)> {
    let remote = remote(binder)?;
    let mut data = remote.prepare_transact(true)?;
    data.write(&set)?;
    let mut reply = remote
        .submit_transact(FORWARD, &data, 0)?
        .ok_or(StatusCode::UnexpectedNull)?;
    reply.set_data_position(0);
    Ok((reply.read()?, reply.read()?))
}

fn serve(name: &str, next: Option<String>) -> Result<()> {
    let probe = Binder::new(Probe { next });
    let server = rsbinder::serve("binder://")?.add(name, Interface::as_binder(&probe))?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn check(front: &str) -> Result<bool> {
    rsbinder::Client::open("binder://")?;
    let front = lookup(front)?;
    let mut ok = true;
    let mut expect = |name: &str, got: String, want: String| {
        if got == want {
            println!("PASS {name}");
        } else {
            ok = false;
            println!("FAIL {name}: got {got}, want {want}");
        }
    };

    expect(
        "nothing set: the server sees unset",
        format!("{:?}", get(&front)?),
        format!("{:?}", (UNSET, false)),
    );

    let token = set_calling_work_source_uid(1234);
    expect(
        "set on a client thread outside any transaction reaches the server",
        format!("{:?}", get(&front)?),
        format!("{:?}", (1234, false)),
    );
    expect(
        "a received work source is not forwarded unless the handler sets it",
        format!("{:?}", forward(&front, NO_SET)?),
        format!("{:?}", (1234, UNSET)),
    );
    expect(
        "a handler that sets a work source forwards its own",
        format!("{:?}", forward(&front, 4321)?),
        format!("{:?}", (1234, 4321)),
    );

    clear_propagate_work_source();
    expect(
        "clear_propagate stops the header carrying it",
        format!("{:?}", get(&front)?),
        format!("{:?}", (UNSET, false)),
    );
    restore_calling_work_source(token);

    // Every server thread either served one of the calls above with a
    // work source installed or never ran; none may carry it into this one.
    for i in 0..8 {
        expect(
            &format!("no leak into a later call ({i})"),
            format!("{:?}", get(&front)?),
            format!("{:?}", (UNSET, false)),
        );
    }
    Ok(ok)
}

fn get_once(name: &str, uid: &str) -> Result<(i32, bool)> {
    rsbinder::Client::open("binder://")?;
    if uid != "unset" {
        set_calling_work_source_uid(uid.parse().map_err(|_| StatusCode::BadValue)?);
    }
    get(&lookup(name)?)
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let usage = || -> ! {
        eprintln!(
            "usage: work_source_probe serve <name> [<next>] | check <front> | get <name> <uid|unset>"
        );
        std::process::exit(2)
    };
    if let (Some("get"), 4) = (args.get(1).map(String::as_str), args.len()) {
        match get_once(&args[2], &args[3]) {
            Ok((uid, propagate)) => {
                println!("RESULT rs get {} SEEN {uid} {}", args[3], propagate as i32)
            }
            Err(e) => {
                println!("RESULT rs get {} ERROR {e:?}", args[3]);
                std::process::exit(1)
            }
        }
        return;
    }
    let result = match (args.get(1).map(String::as_str), args.len()) {
        (Some("serve"), 3 | 4) => serve(&args[2], args.get(3).cloned()).map(|()| true),
        (Some("check"), 3) => check(&args[2]),
        _ => usage(),
    };
    match result {
        Ok(true) => println!("RESULT ok"),
        Ok(false) => {
            println!("RESULT fail");
            std::process::exit(1)
        }
        Err(e) => {
            println!("RESULT fail {e:?}");
            std::process::exit(1)
        }
    }
}

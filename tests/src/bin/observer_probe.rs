// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Transaction observer probe (Plan 10-9 AC-9.5/9.6 kernel half).
//!
//! ```text
//! observer_probe serve <name>   # installs an observer, serves ITraceDemo
//! observer_probe check <name>   # calls it, compares what the observer saw
//! ```
//!
//! The observer panics while observing `ping` (the call must still
//! succeed) and, while observing `add`, looks its own service up through
//! the service manager: a kernel binder call made from inside the observer
//! on the thread serving the transaction, which must neither deadlock nor
//! trip a thread-state borrow. Lines use the format of
//! `tests/tests/observer_rpc.rs`, the RPC half. `check` ends with
//! `RESULT ok` or `RESULT fail`; `tests/scripts/run_observer_ac.sh` drives it.

use std::any::Any;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rsbinder::observe::{set_observer, TransactionObserver, TxnContext};
use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/trace_demo.rs"));

use tracedemo::ITraceDemo::{BnTraceDemo, ITraceDemo};

const DESCRIPTOR: &str = "tracedemo.ITraceDemo";

fn line(ctx: &TxnContext<'_>, result: &rsbinder::Result<()>, tag: bool) -> String {
    format!(
        "{} oneway={} uid={} pid={} transport={} result={result:?} tag={}",
        ctx.method.unwrap_or("?"),
        ctx.is_oneway,
        ctx.calling_uid,
        ctx.calling_pid,
        if ctx.transport == TransportCaps::KERNEL {
            "kernel"
        } else {
            "rpc"
        },
        if tag { "some" } else { "none" },
    )
}

struct Recorder {
    name: String,
    lines: Arc<Mutex<Vec<String>>>,
}

impl TransactionObserver for Recorder {
    fn on_transact(&self, ctx: &TxnContext<'_>) -> Option<Box<dyn Any + Send>> {
        if ctx.descriptor != DESCRIPTOR {
            return None;
        }
        match ctx.method {
            Some("ping") => panic!("observer bug on ping"),
            Some("add") => {
                let found = hub::check_service(&self.name).is_some();
                self.lines
                    .lock()
                    .unwrap()
                    .push(format!("nested check_service found={found}"));
            }
            _ => {}
        }
        Some(Box::new(()))
    }

    fn on_reply(
        &self,
        ctx: &TxnContext<'_>,
        tag: Option<Box<dyn Any + Send>>,
        result: &rsbinder::Result<()>,
        _elapsed: Duration,
    ) {
        // Skip unnamed codes: meta transactions the hub and `connect` also deliver here.
        if ctx.descriptor == DESCRIPTOR && ctx.method.is_some_and(|m| m != "observed") {
            self.lines
                .lock()
                .unwrap()
                .push(line(ctx, result, tag.is_some()));
        }
    }
}

struct Svc {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Interface for Svc {}

impl ITraceDemo for Svc {
    fn r#ping(&self) -> BinderResult<()> {
        Ok(())
    }
    fn r#add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn r#notify(&self, _s: &str) -> BinderResult<()> {
        Ok(())
    }
    fn r#observed(&self) -> BinderResult<Vec<String>> {
        Ok(self.lines.lock().unwrap().clone())
    }
}

fn serve(name: &str) -> Result<()> {
    let lines = Arc::new(Mutex::new(Vec::new()));
    set_observer(Some(Arc::new(Recorder {
        name: name.to_string(),
        lines: lines.clone(),
    })));
    let svc = BnTraceDemo::new_binder(Svc { lines });
    let server = rsbinder::serve("binder://")?.add(name, svc.as_binder())?;
    println!("SERVING {name}");
    use std::io::Write;
    std::io::stdout().flush().ok();
    server.run()
}

fn check(name: &str) -> Result<bool> {
    let svc: Strong<dyn ITraceDemo> = rsbinder::connect(&format!("binder://{name}"))?;
    let mut ok = true;

    if let Err(e) = svc.ping() {
        ok = false;
        println!("FAIL ping failed under a panicking observer: {e:?}");
    }
    let sum = svc.add(2, 3).map_err(|_| StatusCode::FailedTransaction)?;
    if sum != 5 {
        ok = false;
        println!("FAIL add returned {sum}");
    }
    svc.notify("x").map_err(|_| StatusCode::FailedTransaction)?;

    // `notify` is one-way: poll until the observer has seen its reply.
    let mut seen = Vec::new();
    for _ in 0..100 {
        seen = svc.observed().map_err(|_| StatusCode::FailedTransaction)?;
        if seen.len() == 4 {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let uid = rustix::process::getuid().as_raw();
    let pid = std::process::id();
    let common = format!("uid={uid} pid={pid} transport=kernel result=Ok(())");
    let want = [
        format!("ping oneway=false {common} tag=none"),
        "nested check_service found=true".to_string(),
        format!("add oneway=false {common} tag=some"),
        // The driver reports no sender pid for a one-way transaction.
        format!("notify oneway=true uid={uid} pid=0 transport=kernel result=Ok(()) tag=some"),
    ];
    for (i, want) in want.iter().enumerate() {
        match seen.get(i) {
            Some(got) if got == want => println!("PASS {got}"),
            got => {
                ok = false;
                println!("FAIL line {i}: got {got:?}, want {want}");
            }
        }
    }
    if seen.len() > want.len() {
        ok = false;
        println!("FAIL extra lines: {:?}", &seen[want.len()..]);
    }
    Ok(ok)
}

fn main() {
    env_logger::init();
    let args: Vec<String> = std::env::args().collect();
    let result = match (args.get(1).map(String::as_str), args.len()) {
        (Some("serve"), 3) => serve(&args[2]).map(|()| true),
        (Some("check"), 3) => check(&args[2]),
        _ => {
            eprintln!("usage: observer_probe serve <name> | check <name>");
            std::process::exit(2)
        }
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

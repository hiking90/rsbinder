// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 10-10b kernel tests: an `IRpcSmoke` registered with the service
//! manager as `argv[1]`, answering `echo` with `argv[2]` as its tag, so a
//! test can kill and restart it and tell the instances apart. Prints
//! `ready` once registered.

use rsbinder::{hub, Interface, ProcessState, Status};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

struct Svc {
    tag: String,
}
impl Interface for Svc {}
impl IRpcSmoke for Svc {
    fn r#echo(&self, s: &str) -> rsbinder::BinderResult<String> {
        Ok(format!("{}:{s}", self.tag))
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::BinderResult<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::BinderResult<()> {
        Ok(())
    }
    fn r#fail(&self, code: i32) -> rsbinder::BinderResult<()> {
        Err(Status::new_service_specific_error(code, None))
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let name = args.next().expect("usage: reconnect_service NAME TAG");
    let tag = args.next().expect("usage: reconnect_service NAME TAG");
    ProcessState::init_default().expect("ProcessState");
    ProcessState::start_thread_pool();
    hub::add_service(&name, BnRpcSmoke::new_binder(Svc { tag }).as_binder()).expect("add_service");
    println!("ready");
    ProcessState::join_thread_pool().expect("join_thread_pool");
}

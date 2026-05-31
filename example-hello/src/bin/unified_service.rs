// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 Phase D — the **same** service, registration, and serve code
//! over kernel binder *or* RPC, chosen by one line.
//!
//! The `IHello` impl, the `register<R: Registry>` helper, and the call to
//! it are transport-agnostic. Only the host constructor differs:
//!
//! ```text
//! # kernel binder (needs /dev/binder + a running service manager, e.g. rsb_hub)
//! cargo run -p example-hello --features rpc --bin unified_service kernel
//! # RPC over a Unix-domain socket (no kernel binder, no service manager)
//! cargo run -p example-hello --features rpc --bin unified_service rpc
//! ```
//!
//! Pair with `unified_client` using the matching transport argument.

use env_logger::Env;
use example_hello::*;
use rsbinder::service::{kernel, rpc, Registry};
use rsbinder::*;

const RPC_SOCKET: &str = "/tmp/rsb_unified.sock";

struct IHelloService;
impl Interface for IHelloService {}
impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        println!("unified_service: echo({echo:?})");
        Ok(echo.to_owned())
    }
}

/// Transport-agnostic registration — written once, generic over the
/// [`Registry`] trait. Identical for kernel binder and RPC.
fn register<R: Registry>(reg: &R) -> rsbinder::Result<()> {
    let binder = BnHello::new_binder(IHelloService {}).as_binder();
    reg.add_service(SERVICE_NAME, binder)
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    match std::env::args().nth(1).as_deref() {
        Some("kernel") => {
            // One line picks kernel binder. `serve()` joins the
            // process-wide thread pool.
            let host = kernel::Host::new()?;
            register(&host)?;
            println!("unified_service: serving {SERVICE_NAME} over kernel binder");
            host.serve()?;
        }
        Some("rpc") => {
            // One line picks RPC. `serve()` drives this one socket.
            let host = rpc::Host::unix(RPC_SOCKET)?;
            register(&host)?;
            println!("unified_service: serving {SERVICE_NAME} over RPC at {RPC_SOCKET}");
            host.serve()?;
        }
        _ => {
            eprintln!("usage: unified_service <kernel|rpc>");
            std::process::exit(2);
        }
    }
    Ok(())
}

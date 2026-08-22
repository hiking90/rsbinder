// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The **same** service, registration, and serve code over kernel binder
//! *or* RPC — only the URI differs.
//!
//! ```text
//! # kernel binder (needs /dev/binder + a running service manager, e.g. rsb_hub)
//! cargo run -p example-hello --features rpc --bin unified_service kernel
//! # RPC over a Unix-domain socket (no kernel binder, no service manager)
//! cargo run -p example-hello --features rpc --bin unified_service rpc
//! # or any endpoint URI directly
//! cargo run -p example-hello --features rpc --bin unified_service unix:///tmp/x.sock
//! ```
//!
//! Pair with `unified_client` using the matching transport argument.

use env_logger::Env;
use example_hello::*;
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

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // `kernel` / `rpc` are shorthands; anything else is taken as a URI.
    let uri = match std::env::args().nth(1).as_deref() {
        Some("kernel") => "binder://".to_string(),
        Some("rpc") => format!("unix://{RPC_SOCKET}"),
        Some(uri) => uri.to_string(),
        None => {
            eprintln!("usage: unified_service <kernel|rpc|URI>");
            std::process::exit(2);
        }
    };
    println!("unified_service: serving {SERVICE_NAME} at {uri}");
    // Kernel: init + thread pool + service manager + join. RPC: bind the
    // socket, publish, accept loop. Same three calls either way.
    rsbinder::serve(&uri)?
        .add(SERVICE_NAME, BnHello::new_binder(IHelloService {}))?
        .run()?;
    Ok(())
}

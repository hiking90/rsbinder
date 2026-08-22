// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The client counterpart of `unified_service`: one `connect` call,
//! transport chosen by the URI.
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin unified_client kernel
//! cargo run -p example-hello --features rpc --bin unified_client rpc
//! cargo run -p example-hello --features rpc --bin unified_client unix:///tmp/x.sock
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::*;

const RPC_SOCKET: &str = "/tmp/rsb_unified.sock";

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let base = match std::env::args().nth(1).as_deref() {
        Some("kernel") => "binder://".to_string(),
        Some("rpc") => format!("unix://{RPC_SOCKET}"),
        Some(uri) => uri.to_string(),
        None => {
            eprintln!("usage: unified_client <kernel|rpc|URI>");
            std::process::exit(2);
        }
    };
    // `connect` = open the endpoint + look the name up + interface cast.
    // The proxy alone keeps an RPC session alive.
    let hello: Strong<dyn IHello> = rsbinder::connect(&format!("{base}#{SERVICE_NAME}"))?;
    let reply = hello.echo("hello over rsbinder::connect")?;
    println!("unified_client: echo -> {reply:?}");
    Ok(())
}

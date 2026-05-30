// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 Phase D — the client counterpart of `unified_service`.
//!
//! The `talk<B: Broker>` helper (look up + cast + call) is
//! transport-agnostic; only the broker constructor differs:
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin unified_client kernel
//! cargo run -p example-hello --features rpc --bin unified_client rpc
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::service::{kernel, rpc, Broker};
use rsbinder::*;

const RPC_SOCKET: &str = "/tmp/rsb_unified.sock";

/// Transport-agnostic lookup + call — written once, generic over the
/// [`Broker`] trait. `get_interface` is the `interface_cast` step.
fn talk<B: Broker>(broker: &B) -> rsbinder::Result<()> {
    let hello: Strong<dyn IHello> = broker.get_interface(SERVICE_NAME)?;
    let reply = hello.echo("hello over the facade")?;
    println!("unified_client: echo -> {reply:?}");
    Ok(())
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    match std::env::args().nth(1).as_deref() {
        Some("kernel") => {
            let broker = kernel::Broker::new()?;
            talk(&broker)?;
        }
        Some("rpc") => {
            let broker = rpc::Broker::unix(RPC_SOCKET)?;
            talk(&broker)?;
        }
        _ => {
            eprintln!("usage: unified_client <kernel|rpc>");
            std::process::exit(2);
        }
    }
    Ok(())
}

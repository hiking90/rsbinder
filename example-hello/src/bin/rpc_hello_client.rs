// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC variant of `hello_client` — connects to `rpc_hello_service`
//! over a Unix-domain socket and calls the **same generated `IHello`
//! proxy**. The generated stub resolves the RPC binder through
//! `as_remote()` (single-stub): the call site is identical
//! to the kernel path — there is no RPC-specific proxy. `connect`
//! resolves the service by name from the session's in-process directory
//! (named-service model).
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin rpc_hello_service
//! cargo run -p example-hello --features rpc --bin rpc_hello_client
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::Strong;

/// Unix-domain socket `rpc_hello_service` binds.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

/// Must match `rpc_hello_service`'s `SERVICE_NAME`.
const SERVICE_NAME: &str = "hello";

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // `connect` opens the session, looks the name up and casts — the
    // proxy keeps the session alive; nothing else to hold.
    let hello: Strong<dyn IHello> =
        rsbinder::connect(&format!("unix://{RPC_SOCKET}#{SERVICE_NAME}"))?;

    let reply = hello.echo("Hello over RPC!")?;
    println!("rpc_hello_client: server replied {reply:?}");
    assert_eq!(reply, "Hello over RPC!");
    Ok(())
}

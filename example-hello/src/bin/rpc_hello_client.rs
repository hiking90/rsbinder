// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC variant of `hello_client` — connects to `rpc_hello_service`
//! over a Unix-domain socket and calls the **same generated `IHello`
//! proxy**. The generated stub resolves the RPC binder through
//! `as_remote()` (single-stub): the call site is identical
//! to the kernel path — there is no RPC-specific proxy. The
//! `rpc::Broker` facade resolves the service by name from the session's
//! in-process directory (named-service model).
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin rpc_hello_service
//! cargo run -p example-hello --features rpc --bin rpc_hello_client
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::service::{rpc, Broker as _};
use rsbinder::Strong;

/// Unix-domain socket `rpc_hello_service` binds.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

/// Must match `rpc_hello_service`'s `SERVICE_NAME`.
const SERVICE_NAME: &str = "hello";

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // `rpc::Broker` owns the `RpcSession` — keep it alive for the
    // duration of the calls. `get_interface` does the lookup + the
    // `interface_cast` (`try_from`) in one step.
    let broker = rpc::Broker::unix(RPC_SOCKET)?;
    let hello: Strong<dyn IHello> = broker.get_interface(SERVICE_NAME)?;

    let reply = hello.echo("Hello over RPC!")?;
    println!("rpc_hello_client: server replied {reply:?}");
    assert_eq!(reply, "Hello over RPC!");
    Ok(())
}

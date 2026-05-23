// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC variant of `hello_client` — connects to `rpc_hello_service`
//! over a Unix-domain socket and calls the **same generated `IHello`
//! proxy**. The generated stub resolves the RPC binder through
//! `as_remote()` (Plan 2-6.B single-stub): the call site is identical
//! to the kernel path — there is no RPC-specific proxy and no service
//! manager (the root object comes straight off the session).
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin rpc_hello_service
//! cargo run -p example-hello --features rpc --bin rpc_hello_client
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::rpc::RpcSession;
use rsbinder::{FromIBinder, Strong};

/// Unix-domain socket `rpc_hello_service` binds.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let session = RpcSession::setup_unix_client(RPC_SOCKET)?;
    let root = session.get_root()?;

    // One generated stub for both stacks: `try_from` →
    // `Proxy::from_binder` (stamps the RPC descriptor in place) →
    // generated methods emit `as_remote().ok_or(BadType)?`.
    let hello: Strong<dyn IHello> = <dyn IHello as FromIBinder>::try_from(root)?;

    let reply = hello.echo("Hello over RPC!")?;
    println!("rpc_hello_client: server replied {reply:?}");
    assert_eq!(reply, "Hello over RPC!");
    Ok(())
}

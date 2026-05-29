// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! RPC (binder-over-socket) variant of `hello_service` — serves the
//! **same generated `IHello` stub** over a Unix-domain socket instead
//! of the kernel binder driver. No `/dev/binder`, no service manager:
//! the RPC transport is a separate, parallel stack. Pair it
//! with `rpc_hello_client`:
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin rpc_hello_service
//! cargo run -p example-hello --features rpc --bin rpc_hello_client
//! ```

use env_logger::Env;
use example_hello::*;
use rsbinder::rpc::RpcServer;
use rsbinder::*;

/// Unix-domain socket the server binds and the client connects to.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

struct IHelloService;

impl Interface for IHelloService {}

impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        println!("rpc_hello_service: echo({echo:?})");
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // No `ProcessState` / `hub`: RPC does not touch the kernel binder
    // singleton. Bind a UDS, publish the generated `BnHello` as the
    // session's root object, then serve until killed.
    let _ = std::fs::remove_file(RPC_SOCKET);
    let server = RpcServer::setup_unix_server(RPC_SOCKET)?;
    server.set_root(BnHello::new_binder(IHelloService {}).as_binder());

    println!("rpc_hello_service listening on {RPC_SOCKET}");
    server.run()?;
    Ok(())
}

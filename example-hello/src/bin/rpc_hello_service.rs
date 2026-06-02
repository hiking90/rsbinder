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
use rsbinder::service::{rpc, Registry as _};
use rsbinder::*;

/// Unix-domain socket the server binds and the client connects to.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

/// Name the service is published under on the RPC session's in-process
/// directory; the client resolves the same name. The RPC facade uses the
/// named-service model (`add_service`/`get_interface`), not the bare root
/// object.
const SERVICE_NAME: &str = "hello";

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
    // singleton. The `rpc::Host` facade binds the UDS; `add_service`
    // publishes the generated `BnHello` under a name, and `serve()`
    // drives this one socket until killed.
    let _ = std::fs::remove_file(RPC_SOCKET);
    let host = rpc::Host::unix(RPC_SOCKET)?;
    host.add_service(
        SERVICE_NAME,
        BnHello::new_binder(IHelloService {}).as_binder(),
    )?;

    println!("rpc_hello_service listening on {RPC_SOCKET}");
    host.serve()?;
    Ok(())
}

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
use rsbinder::*;

/// Unix-domain socket the server binds and the client connects to.
const RPC_SOCKET: &str = "/tmp/rsb_hello_rpc.sock";

/// Name the service is published under on the RPC session's in-process
/// directory; the client resolves the same name (`serve().add` /
/// `connect` use the named-service model, not the bare root object).
const SERVICE_NAME: &str = "hello";

struct IHelloService;

impl Interface for IHelloService {}

impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        println!("rpc_hello_service: echo({echo:?})");
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // No `ProcessState` / `hub`: RPC does not touch the kernel binder
    // singleton. `serve("unix://…")` takes the UDS, `add` publishes the
    // generated `BnHello` under a name, and the server drives this one
    // socket until killed — the same three calls as `serve("binder://")`.
    // `spawn` binds before returning, so the line below is only printed
    // once the socket really exists.
    let _guard = rsbinder::serve(&format!("unix://{RPC_SOCKET}"))?
        .add(SERVICE_NAME, BnHello::new_binder(IHelloService {}))?
        .spawn()?;
    println!("rpc_hello_service listening on {RPC_SOCKET}");
    std::thread::park();
    Ok(())
}

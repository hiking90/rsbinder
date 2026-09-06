// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A **gateway**: connect to `IHello` on one transport and re-publish it
//! on another. The whole bridge is the `BnHello::new_binder(upstream)`
//! below — a typed handle implements the interface it points at, so a
//! proxy satisfies `new_binder`'s bound and becomes a local binder this
//! process owns and can publish anywhere.
//!
//! This is the answer to "a socket client needs a kernel service"; the
//! reverse direction is the Accessor (see the book). It is *not* a
//! transparent bridge — see `book/src/cross-transport-services.md` for
//! what stops at the gateway (caller identity, fds, uid, death, one
//! worker per forwarded call).
//!
//! ```text
//! # 1. C — the real service, on kernel binder
//! cargo run -p example-hello --bin hello_service
//!
//! # 2. B — this gateway
//! cargo run -p example-hello --features rpc --bin gateway_service \
//!     binder://my.hello unix:///tmp/rsb_gw.sock
//!
//! # 3. A — an ordinary client, which never learns C exists
//! cargo run -p example-hello --features rpc --bin unified_client \
//!     unix:///tmp/rsb_gw.sock
//! ```
//!
//! Either URI may name either transport, so the same binary also fronts
//! an RPC service on kernel binder (`unix:///tmp/x.sock#my.hello
//! binder://`) or bridges two sockets.

use env_logger::Env;
use example_hello::*;
use rsbinder::Strong;

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let mut args = std::env::args().skip(1);
    let (Some(upstream_uri), Some(listen_uri)) = (args.next(), args.next()) else {
        eprintln!(
            "usage: gateway_service <upstream-uri> <listen-uri>\n\
             \n\
             e.g. gateway_service binder://{SERVICE_NAME} unix:///tmp/rsb_gw.sock\n\
             \n\
             The upstream URI names the service to front; the listen URI is\n\
             where this process re-publishes it under the same name."
        );
        std::process::exit(2);
    };

    // The upstream service, reached however its URI says.
    let upstream: Strong<dyn IHello> = rsbinder::connect(&upstream_uri)?;
    println!("gateway_service: upstream {upstream_uri} is up");

    // The bridge. `upstream` is a proxy, but `Strong<dyn IHello>`
    // implements `IHello`, so `new_binder` accepts it and hands back a
    // *local* binder that forwards every call.
    //
    // Handing the proxy over directly — `.add(SERVICE_NAME,
    // upstream.as_binder())` — is refused with `InvalidOperation`: a
    // binder of one stack cannot be published on the other.
    rsbinder::serve(&listen_uri)?
        .add(SERVICE_NAME, BnHello::new_binder(upstream))?
        .run()?;

    Ok(())
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client for `authz_service` (Plan 2-16 handler-side authorization).
//!
//! Calls both methods and prints the outcome: `whoami()` is authorized
//! (the handler reports this connection's uid), while `adminOnly()` is
//! denied with `EX_SECURITY` for a normal (non-root) user.
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin authz_service
//! cargo run -p example-hello --features rpc --bin authz_client
//! ```

use env_logger::Env;
use example_hello::authz::*;
use rsbinder::rpc::RpcSession;
use rsbinder::{FromIBinder, Strong};

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let session = RpcSession::setup_unix_client(RPC_SOCKET)?;
    let authz: Strong<dyn IAuthz> = FromIBinder::try_from(session.get_root()?)?;

    // Allowed: an identifiable Unix-RPC peer (this process).
    match authz.whoami() {
        Ok(who) => println!("whoami    -> OK: {who}"),
        Err(e) => println!("whoami    -> DENIED ({:?})", e.exception_code()),
    }

    // Denied: requires uid 0, and we are not root.
    match authz.adminOnly() {
        Ok(msg) => println!("adminOnly -> OK: {msg}"),
        Err(e) => println!("adminOnly -> DENIED ({:?})", e.exception_code()),
    }

    Ok(())
}

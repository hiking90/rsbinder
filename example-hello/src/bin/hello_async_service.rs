// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Async counterpart of `hello_service`.
//!
//! The service implementation is `async` (an `IHelloAsyncService`), and
//! `BnHello::new_async_binder` bridges it onto the synchronous binder dispatch
//! by driving each inbound call with `rt.block_on(..)`. Run it alongside
//! `hello_async_client` (or the sync `hello_client`, since the wire is identical).

use async_trait::async_trait;
use env_logger::Env;
use example_hello::*;
use rsbinder::*;

// The struct must NOT be named `IHelloAsyncService` — that is the generated
// trait. The impl type is just a plain service object.
struct HelloAsyncImpl;

impl Interface for HelloAsyncImpl {}

#[async_trait]
impl IHelloAsyncService for HelloAsyncImpl {
    async fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        // A real await point would go here in a non-trivial service.
        Ok(echo.to_owned())
    }
}

// Multi-threaded, which is `#[tokio::main]`'s default flavor. A current-thread
// runtime completes handlers only while this thread stays parked inside
// `Runtime::block_on`, and even parked it cannot reach a second local service
// through a sync handle from inside a handler. See the async chapter of the book.
#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // The runtime `block_on` runs each inbound call against. Binder threads are
    // outside it, which is what `Handle::block_on` wants.
    let rt = TokioRuntime(tokio::runtime::Handle::current());
    let service = BnHello::new_async_binder(HelloAsyncImpl, rt);

    // Same three calls as `hello_service`, except `spawn` instead of `run`:
    // for kernel binder it starts the thread pool and returns, leaving this
    // task free to await. (`run` would join the pool and block this thread,
    // which a current-thread runtime could not survive.) The kernel grows the
    // pool from there as load arrives.
    println!("Serving {SERVICE_NAME} over kernel binder (async)...");
    let _guard = rsbinder::serve("binder://")?
        .add(SERVICE_NAME, &service)?
        .spawn()?;

    // Park until killed. To shut down on Ctrl-C instead, await
    // `tokio::signal::ctrl_c()` here and declare `tokio/signal` in this
    // crate's own Cargo.toml — rsbinder does not bring that feature in.
    std::future::pending::<()>().await;
    Ok(())
}

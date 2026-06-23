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

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    // Bridge the async impl onto sync binder dispatch; rt.block_on drives each call.
    let rt = TokioRuntime(tokio::runtime::Handle::current());
    let service = BnHello::new_async_binder(HelloAsyncImpl, rt);

    hub::add_service(SERVICE_NAME, &service)?;
    println!("Registered async service: {SERVICE_NAME}");

    // `join_thread_pool` blocks forever; run it off the runtime so the Tokio
    // worker threads stay free to drive the async handlers.
    tokio::task::spawn_blocking(ProcessState::join_thread_pool).await??;
    Ok(())
}

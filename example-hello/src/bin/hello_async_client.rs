// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

//! Async counterpart of `hello_client`: obtains the proxy and calls it with
//! `.await`. Works against either `hello_async_service` or the sync
//! `hello_service` (the wire is identical).

use env_logger::Env;
use example_hello::*;
use rsbinder::*;

// Type alias keeps the `<Tokio>` turbofish out of every use site.
type IHelloAsyncTokio = dyn IHelloAsync<Tokio>;

#[tokio::main]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    // SM lookups are sync; run off the runtime, then upgrade to the async view.
    let hello: Strong<IHelloAsyncTokio> =
        tokio::task::spawn_blocking(|| hub::wait_for_interface::<dyn IHello>(SERVICE_NAME))
            .await??
            .into_async::<Tokio>();

    // The call now returns a future.
    let echo = hello.echo("Hello (async) World!").await?;
    println!("Result: {echo}");

    Ok(())
}

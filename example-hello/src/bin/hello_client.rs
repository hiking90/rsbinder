// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use example_hello::*;
use rsbinder::*;
use std::sync::Arc;

struct MyDeathRecipient {}

impl DeathRecipient for MyDeathRecipient {
    fn binder_died(&self, _who: &WIBinder) {
        println!("MyDeathRecipient");
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with the default binder path and the default max threads.
    ProcessState::init_default()?;

    // Start the thread pool so this client's inbound transactions — the
    // event-driven `onRegistration` (wait_for_interface) and the death
    // notification below — are delivered promptly; without it the wait still
    // works but degrades to ~1s polling. See `ProcessState::start_thread_pool`.
    ProcessState::start_thread_pool();

    println!("list services:");
    // This is an example of how to use service manager.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{name}");
    }

    // Block until the Hello service is registered, then cast it to the
    // interface — the event-driven AOSP `waitForService` equivalent. This
    // replaces the old hand-rolled retry loop: no polling, no fixed attempt
    // cap, and the register-after-miss race is handled by the hub.
    let hello: rsbinder::Strong<dyn IHello> = hub::wait_for_interface(SERVICE_NAME)?;

    let recipient = Arc::new(MyDeathRecipient {});
    hello
        .as_binder()
        .link_to_death(Arc::downgrade(&(recipient as Arc<dyn DeathRecipient>)))?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    Ok(ProcessState::join_thread_pool()?)
}

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

    // `Client::open` starts the binder thread pool, so the death
    // notification registered below is delivered promptly.
    let client = rsbinder::Client::open("binder://")?;

    println!("list services:");
    // Kernel-only service-manager powers stay on `hub`.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{name}");
    }

    // Blocks until the service is registered (AOSP `waitForService`).
    let hello: rsbinder::Strong<dyn IHello> = client.get(SERVICE_NAME)?;

    // `link_to_death_arc` takes the concrete `Arc<MyDeathRecipient>` directly —
    // no `Arc::downgrade(&(x as Arc<dyn DeathRecipient>))` incantation. Keep
    // `recipient` alive for the link's lifetime (the link holds only a weak ref).
    let recipient = Arc::new(MyDeathRecipient {});
    hello.link_to_death_arc(&recipient)?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    Ok(ProcessState::join_thread_pool()?)
}

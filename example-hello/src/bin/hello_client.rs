// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use example_hello::*;
use rsbinder::*;
use std::sync::Arc;
use std::time::Duration;

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

    println!("list services:");
    // This is an example of how to use service manager.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{name}");
    }

    // Create a Hello proxy from the binder service manager. If this client
    // starts before the service has finished registering, the lookup fails, so
    // retry a few times before giving up and surfacing the real Status error.
    let hello: rsbinder::Strong<dyn IHello> = {
        const RETRIES: u32 = 5;
        let mut attempt = 0;
        loop {
            match hub::get_interface(SERVICE_NAME) {
                Ok(hello) => break hello,
                Err(e) if attempt < RETRIES => {
                    attempt += 1;
                    std::thread::sleep(Duration::from_millis(500));
                    println!("Waiting for {SERVICE_NAME}... (retry {attempt}/{RETRIES}): {e}");
                }
                Err(e) => return Err(e.into()),
            }
        }
    };

    let recipient = Arc::new(MyDeathRecipient {});
    hello
        .as_binder()
        .link_to_death(Arc::downgrade(&(recipient as Arc<dyn DeathRecipient>)))?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    Ok(ProcessState::join_thread_pool()?)
}

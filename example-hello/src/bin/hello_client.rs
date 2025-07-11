// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
#![allow(non_snake_case)]

use env_logger::Env;
use example_hello::*;
use hub::{BnServiceCallback, IServiceCallback};
use rsbinder::*;
use std::sync::Arc;

struct MyServiceCallback {}

impl Interface for MyServiceCallback {}

impl IServiceCallback for MyServiceCallback {
    fn onRegistration(&self, name: &str, _service: &SIBinder) -> rsbinder::status::Result<()> {
        println!("MyServiceCallback: {name}");
        Ok(())
    }
}

struct MyDeathRecipient {}

impl DeathRecipient for MyDeathRecipient {
    fn binder_died(&self, _who: &WIBinder) {
        println!("MyDeathRecipient");
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with the default binder path and the default max threads.
    ProcessState::init_default();

    println!("list services:");
    // This is an example of how to use service manager.
    for name in hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT) {
        println!("{name}");
    }

    let service_callback = BnServiceCallback::new_binder(MyServiceCallback {});
    hub::register_for_notifications(SERVICE_NAME, &service_callback)?;

    // Create a Hello proxy from binder service manager.
    let hello: rsbinder::Strong<dyn IHello> =
        hub::get_interface(SERVICE_NAME).unwrap_or_else(|_| panic!("Can't find {SERVICE_NAME}"));

    let recipient = Arc::new(MyDeathRecipient {});
    hello
        .as_binder()
        .link_to_death(Arc::downgrade(&(recipient as Arc<dyn DeathRecipient>)))?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    Ok(ProcessState::join_thread_pool()?)
}

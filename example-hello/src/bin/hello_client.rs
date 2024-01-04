// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::Arc;
use rsbinder_hub::{IServiceManager, IServiceCallback, BnServiceCallback};
use env_logger::Env;
use rsbinder::*;
use example_hello::*;

struct MyServiceCallback {
}

impl Interface for MyServiceCallback {}

impl IServiceCallback for MyServiceCallback {
    fn onRegistration(&self, name: &str, service: &SIBinder) -> rsbinder::status::Result<()> {
        println!("MyServiceCallback: {name}");
        Ok(())
    }
}

struct MyDeathRecipient {
}

impl DeathRecipient for MyDeathRecipient {
    fn binder_died(&self, who: WIBinder) {
        println!("MyDeathRecipient");
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with binder path and max threads.
    // The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    ProcessState::init(DEFAULT_BINDER_PATH, 0);
    // Get binder service manager.
    let hub = rsbinder_hub::default();

    println!("list services:");
    // This is an example of how to use service manager.
    for name in hub.listServices(rsbinder_hub::DUMP_FLAG_PRIORITY_DEFAULT)? {
        println!("{}", name);
    }

    let service_callback = BnServiceCallback::new_binder(MyServiceCallback{});
    hub.registerForNotifications(SERVICE_NAME, &service_callback)?;

    // Create a Hello proxy from binder service manager.
    let hello = BpHello::from_binder(rsbinder_hub::get_service(SERVICE_NAME)
        .expect(&format!("Can't find {SERVICE_NAME}")))?;

    hello.as_binder().link_to_death(Arc::new(MyDeathRecipient{}))?;

    // Call echo method of Hello proxy.
    let echo = hello.echo("Hello World!")?;

    println!("Result: {echo}");

    // sleep 1 second
    // std::thread::sleep(std::time::Duration::from_secs(1));

    Ok(ProcessState::join_thread_pool()?)
}
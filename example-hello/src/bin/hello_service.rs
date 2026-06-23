// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use env_logger::Env;
use rsbinder::*;

use example_hello::*;

// Define the name of the service to be registered in the HUB(service manager).
struct IHelloService;

// Implement the IHello interface for the IHelloService.
impl Interface for IHelloService {
    // Reimplement the dump method. This is optional.
    fn dump(&self, writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        writeln!(writer, "Dump IHelloService")?;
        Ok(())
    }
}

// Implement the IHello interface for the IHelloService.
impl IHello for IHelloService {
    // Implement the echo method.
    fn echo(&self, echo: &str) -> rsbinder::BinderResult<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with the default binder path and the default max threads.
    println!("Initializing ProcessState...");
    ProcessState::init_default()?;

    // Start the thread pool — required for any service that handles incoming
    // calls: it lets the kernel add workers, so a re-entrant or concurrent call
    // won't block on the lone `join_thread_pool` thread below. See
    // `ProcessState::start_thread_pool` for the precise rule.
    println!("Starting thread pool...");
    ProcessState::start_thread_pool();

    // Create a binder service.
    println!("Creating service...");
    let service = BnHello::new_binder(IHelloService {});
    // Alternative: opt into receiving the caller's SELinux security
    // context (read via `CallingContext::default().sid` in transactions):
    //
    //     use rsbinder::BinderFeatures;
    //     let mut features = BinderFeatures::default();
    //     features.set_requesting_sid = true;
    //     let service = BnHello::new_binder_with_features(IHelloService {}, features);

    // Add the service to binder service manager. `add_service` takes anything
    // convertible into `SIBinder`, so the typed handle goes in directly — pass
    // `&service` to keep the local handle alive for the rest of `main`.
    println!("Adding service to hub...");
    hub::add_service(SERVICE_NAME, &service)?;

    // Join the thread pool.
    // This is a blocking call. It will return when the thread pool is terminated.
    Ok(ProcessState::join_thread_pool()?)
}

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
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with the default binder path and the default max threads.
    println!("Initializing ProcessState...");
    ProcessState::init_default();

    // Start the thread pool.
    // This is optional. If you don't call this, only one thread will be created to handle the binder transactions.
    println!("Starting thread pool...");
    ProcessState::start_thread_pool();

    // Create a binder service.
    println!("Creating service...");
    let service = BnHello::new_binder(IHelloService{});

    // Add the service to binder service manager.
    println!("Adding service to hub...");
    hub::add_service(SERVICE_NAME, service.as_binder())?;

    // Join the thread pool.
    // This is a blocking call. It will return when the thread pool is terminated.
    Ok(ProcessState::join_thread_pool()?)
}

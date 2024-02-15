// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use rsbinder_hub::IServiceManager;
use env_logger::Env;
use rsbinder::*;

use example_hello::*;

struct IHelloService;

impl Interface for IHelloService {
    fn dump(&self, writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        writeln!(writer, "Dump IHelloService")?;
        Ok(())
    }
}

impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    // Initialize ProcessState with binder path and max threads.
    // The meaning of zero max threads is to use the default value. It is dependent on the kernel.
    ProcessState::init(DEFAULT_BINDER_PATH, 0);

    // Create a binder service.
    let service = BnHello::new_binder(IHelloService{});

    // Add the service to binder service manager.
    let hub = rsbinder_hub::default();
    hub.addService(SERVICE_NAME, &service.as_binder(), false, rsbinder_hub::DUMP_FLAG_PRIORITY_DEFAULT)?;

    Ok(ProcessState::join_thread_pool()?)
}

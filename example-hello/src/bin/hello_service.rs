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

    // `serve("binder://")` initializes the kernel binder `ProcessState`;
    // `add` registers with the service manager (anything `Into<SIBinder>`
    // — pass `&service` to keep the local handle alive); `run` starts the
    // thread pool and joins it. The same three calls with
    // `serve("unix:///tmp/hello.sock")` would serve over RPC instead.
    println!("Serving {SERVICE_NAME} over kernel binder...");
    rsbinder::serve("binder://")?
        .add(SERVICE_NAME, &service)?
        .run()?;
    Ok(())
}

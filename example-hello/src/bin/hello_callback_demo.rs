// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Lazy service: registers `SERVICE_NAME` through `LazyServiceRegistrar` and
// **exits by itself** once nothing is using it. That is the whole point of
// the pattern — the service manager starts the process again on the next
// lookup, so an idle service costs nothing.
//
// The registrar does the service-manager work: `addService` with
// `FLAG_IS_LAZY_SERVICE`, `registerClientCallback`, and the `onClients`
// bookkeeping. Nothing here registers a callback by hand.
//
// Expected trace, paired with rsb_hub and a short-lived `hello_client`:
//   1. T+0        register_service → addService + registerClientCallback.
//                 The service starts out assumed to have clients, so the
//                 process cannot exit before the hub has said anything.
//   2. T+x        `hello_client` looks the service up → the hub reports
//                 clients → `has_clients=true`.
//   3. T+x+1      `hello_client` exits → the kernel ref count drops.
//   4. T+x+(≤5)   the hub's 5-second poller reports `onClients(false)` →
//                 tryUnregisterService → the process exits 0.
//
// Use with:
//   RUST_LOG=info ./hello_callback_demo &
//   sleep 2
//   timeout 3 ./hello_client          # touches my.hello + exits
//   wait                              # the demo exits on its own
//
// `tests/scripts/run_lazy_service_ac.sh` runs exactly that and gates on the
// exit status.
#![allow(non_snake_case)]

use std::sync::Arc;
use std::time::Instant;

use example_hello::*;
use rsbinder::lazy_service::LazyServiceRegistrar;
use rsbinder::*;

struct IHelloService;
impl Interface for IHelloService {}
impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // `register_service` talks to the service manager, so the kernel binder
    // `ProcessState` has to exist first.
    ProcessState::init_default()?;

    let service = BnHello::new_binder(IHelloService {});
    let registrar = LazyServiceRegistrar::new();

    // Observe the transitions without taking the decision away: returning
    // `false` means "not handled", so the registrar still shuts the process
    // down when the count reaches zero.
    let start = Instant::now();
    registrar.set_active_services_callback(Arc::new(move |has_clients| {
        println!(
            "[+{:5.1}s] has_clients={has_clients}",
            start.elapsed().as_secs_f32()
        );
        false
    }));

    registrar.register_service(SERVICE_NAME, service.as_binder())?;
    println!("Registered lazy service: {SERVICE_NAME}");

    // Keep the service binder alive for as long as the process runs.
    let _keep_alive = service;

    // Returns only if the thread pool is torn down; the usual exit is
    // `LazyServiceRegistrar`'s own, from the callback thread.
    ProcessState::join_thread_pool()?;
    Ok(())
}

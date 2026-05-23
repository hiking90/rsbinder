// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Lazy-service-style demo: registers `SERVICE_NAME` and then registers
// *itself* as the `IClientCallback` for that name. Every `onClients`
// transition prints to stdout with a wall-clock offset, so the rsb_hub
// 5-second client-callback poller can be observed end-to-end.
//
// Expected trace when paired with rsb_hub + a short-lived
// `hello_client`:
//   1. T+0   addService + registerClientCallback → first internal
//            `handle_service_client_callback(..., is_called_on_interval=false)`
//            may already emit `onClients(true)` if the kernel ref-count
//            seen by rsb_hub exceeds `KNOWN_CLIENTS=2`.
//   2. T+x   external `hello_client` calls `get_service(SERVICE_NAME)`
//            → rsb_hub sets `guarantee_client=true`. Either fires a
//            fresh `onClients(true)` or is a no-op if state already
//            matches (the latter exercises the A2 "log+return" guard
//            replacing the prior `process::abort()`).
//   3. T+x+1 `hello_client` exits → kernel binder ref-count drops.
//   4. T+x+(≤5)  rsb_hub's 5-second poller wakes and calls
//            `handle_service_client_callback(..., is_called_on_interval=true)`
//            → `has_kernel_reported_clients=false` + `has_clients=true`
//            arm fires `onClients(false)`.
//
// Use with:
//   RUST_LOG=info ./hello_callback_demo &
//   sleep 2
//   timeout 3 ./hello_client          # touches my.hello + exits
//   sleep 8                            # let the 5s poller fire
//   pkill hello_callback_demo
#![allow(non_snake_case)]

use std::time::Instant;

use example_hello::*;
use rsbinder::hub::android_16::android::os::IClientCallback::{BnClientCallback, IClientCallback};
use rsbinder::hub::android_16::IServiceManager;
use rsbinder::*;

struct IHelloService;
impl Interface for IHelloService {}
impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::status::Result<String> {
        Ok(echo.to_owned())
    }
}

struct MyClientCallback {
    start: Instant,
}
impl Interface for MyClientCallback {}
impl IClientCallback for MyClientCallback {
    fn onClients(&self, _registered: &SIBinder, has_clients: bool) -> rsbinder::status::Result<()> {
        let elapsed = self.start.elapsed().as_secs_f32();
        println!("[+{elapsed:5.1}s] onClients(has_clients={has_clients})");
        Ok(())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    let service = BnHello::new_binder(IHelloService {});
    let service_binder = service.as_binder();
    hub::add_service(SERVICE_NAME, service_binder.clone())?;
    println!("Registered service: {SERVICE_NAME}");

    let start = Instant::now();
    let callback = BnClientCallback::new_binder(MyClientCallback { start });

    // `hub::default()` returns the version-dispatching enum. On
    // non-Android the only active arm is `Android16` (cfg-gated enum
    // in `rsbinder/src/hub/mod.rs`) → the match is single-arm
    // exhaustive. On Android the older SDK variants (`Android10`–
    // `Android14`) also compile in, requiring the `cfg`-gated
    // catch-all. The non-Android single-arm form trips
    // `clippy::infallible_destructuring_match` (suggests `let
    // X(bp) = ...`), but that alternative trips
    // `irrefutable_let_patterns` for the same reason — neither lint
    // can accommodate both targets, so we allow the destructuring-
    // match lint only where it fires. We reach into the
    // `BpServiceManager` directly because `registerClientCallback`
    // isn't surfaced as a hub-level convenience function.
    let sm = hub::default()?;
    #[cfg_attr(
        not(target_os = "android"),
        allow(clippy::infallible_destructuring_match)
    )]
    let bp = match &*sm {
        hub::ServiceManager::Android16(bp) => bp,
        #[cfg(target_os = "android")]
        _ => {
            return Err(
                "hello_callback_demo expects the Android 16 ServiceManager variant \
                 (Linux default, or an Android target whose detected SDK is 36)"
                    .into(),
            );
        }
    };
    bp.registerClientCallback(SERVICE_NAME, &service_binder, &callback)
        .map_err(|e| format!("registerClientCallback failed: {e:?}"))?;
    println!(
        "[+{:5.1}s] Registered client callback; awaiting transitions...",
        start.elapsed().as_secs_f32()
    );

    // Keep the service binder alive for the lifetime of the process —
    // the `service` local goes out of scope only when `join_thread_pool`
    // returns (which it normally doesn't).
    let _keep_alive = service;

    Ok(ProcessState::join_thread_pool()?)
}

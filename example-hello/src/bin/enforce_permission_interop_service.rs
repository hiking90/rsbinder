// SPDX-License-Identifier: Apache-2.0
//
// `@EnforcePermission` codegen interop with
// real Android `PermissionManagerService` on the emulator.
//
// Registers `rsbinder.test.permcheck` with `set_requesting_sid = true`.
// Every `IPermCheck` method on the service returns `true` (or an echo)
// unconditionally — the *only* path that can produce
// `Status::Security` for the client is the generated `on_transact`
// check that runs BEFORE the method body. If the client receives
// `Security` for `doDenied()` (whose fabricated permission cannot exist
// in `system_server`'s permission map), the `@EnforcePermission` codegen
// + the `permission_controller::check_permission` proxy
// have round-tripped against real `PermissionManagerService`.

use rsbinder::*;

use example_hello::permcheck::{BnPermCheck, IPermCheck, SERVICE_NAME};

struct PermCheckImpl;

impl Interface for PermCheckImpl {}

impl IPermCheck for PermCheckImpl {
    fn doSingle(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn doAllOf(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn doAnyOf(&self) -> rsbinder::status::Result<bool> {
        Ok(true)
    }
    fn doDenied(&self) -> rsbinder::status::Result<bool> {
        // This must NEVER run — the generated `on_transact` arm should
        // reject before reaching here. We return a sentinel that the
        // client compares against to detect a leak.
        eprintln!("STAGE3_4_2_LEAK: doDenied() body ran — generated check did not fire!");
        Ok(false)
    }
    fn echo(&self, message: &str) -> rsbinder::status::Result<String> {
        Ok(message.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    eprintln!("STAGE3 4-2 server: init ProcessState");
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    let mut features = BinderFeatures::default();
    features.set_requesting_sid = true;
    let service = BnPermCheck::new_binder_with_features(PermCheckImpl, features);

    eprintln!("STAGE3 4-2 server: register `{SERVICE_NAME}`");
    hub::add_service(SERVICE_NAME, service.as_binder())?;

    eprintln!("STAGE3 4-2 server: join thread pool");
    Ok(ProcessState::join_thread_pool()?)
}

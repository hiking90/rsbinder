// SPDX-License-Identifier: Apache-2.0
//
// Server-side smoke for calling-identity
// extraction (`get_calling_*` / `clear_calling_identity` /
// `restore_calling_identity` / `has_explicit_identity`) against real
// Android emulator (kernel binder + servicemanager).
//
// Registers a service that, on each `describeCaller()` transact:
//   1. Reads `get_calling_uid()` / `get_calling_pid()` / `get_calling_sid()`.
//   2. Asserts `has_explicit_identity() == false` (kernel-delivered
//      identity is the baseline, no `clear_calling_identity` yet).
//   3. Calls `clear_calling_identity()` → asserts
//      `has_explicit_identity() == true`.
//   4. Calls `restore_calling_identity(token)` → asserts
//      `has_explicit_identity() == false` again.
//   5. Returns a single human-readable line summarizing all of the above.
//
// The client (`calling_identity_interop_client`) cross-checks the SID against the
// expected SELinux context for the caller's domain (e.g. `u:r:shell:s0`
// when invoked from `adb shell`).

use env_logger::Env;
use rsbinder::*;

use example_hello::calling_identity::{BnCallingIdentity, ICallingIdentity, SERVICE_NAME};

struct CallingIdentitySmoke;

impl Interface for CallingIdentitySmoke {}

impl ICallingIdentity for CallingIdentitySmoke {
    fn describeCaller(&self) -> rsbinder::status::Result<String> {
        let uid = rsbinder::get_calling_uid();
        let pid = rsbinder::get_calling_pid();
        let sid_pre = rsbinder::get_calling_sid();
        let explicit_pre = rsbinder::has_explicit_identity();

        let token = rsbinder::clear_calling_identity();
        let explicit_after_clear = rsbinder::has_explicit_identity();

        rsbinder::restore_calling_identity(token);
        let explicit_after_restore = rsbinder::has_explicit_identity();

        let sid_repr = sid_pre
            .as_ref()
            .and_then(|s| s.to_str().ok())
            .map(|s| s.to_owned())
            .unwrap_or_else(|| "<none>".to_owned());

        Ok(format!(
            "uid={uid} pid={pid} sid={sid_repr} \
             explicit_pre={explicit_pre} \
             explicit_after_clear={explicit_after_clear} \
             explicit_after_restore={explicit_after_restore}"
        ))
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    eprintln!("STAGE3 4-1 server: init ProcessState");
    ProcessState::init_default()?;
    ProcessState::start_thread_pool();

    // Opt the binder into BR_TRANSACTION_SEC_CTX so the kernel delivers
    // the caller's SELinux context.
    let mut features = BinderFeatures::default();
    features.set_requesting_sid = true;
    let service = BnCallingIdentity::new_binder_with_features(CallingIdentitySmoke, features);

    eprintln!("STAGE3 4-1 server: register `{SERVICE_NAME}`");
    hub::add_service(SERVICE_NAME, service.as_binder())?;

    eprintln!("STAGE3 4-1 server: join thread pool");
    Ok(ProcessState::join_thread_pool()?)
}

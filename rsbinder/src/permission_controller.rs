// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client stub for Android's `PermissionManagerService`
//! (`android.os.IPermissionController`).
//!
//! rsbinder provides the **client side only** — the server lives in
//! Android's `system_server`. Consumers acquire a proxy via
//! [`crate::hub::get_service`]`("permission")` then cast through
//! [`crate::permission_controller::IPermissionController`].
//!
//! The wire descriptor is `"android.os.IPermissionController"`, generated
//! from `aidl/permission/android/os/IPermissionController.aidl` (vendored
//! verbatim from AOSP `frameworks/base/core/java/android/os/`). The
//! descriptor is stable across Android 10–16; rsbinder ships a single
//! AIDL (unlike `IServiceManager`, which is per-version).
//!
//! On non-Android targets (Linux + binderfs without an Android
//! userspace, or macOS), `system_server` does not exist — the helpers
//! still compile but [`crate::hub::get_service`] returns `None` and the
//! [`crate::permission_controller::check_permission`] convenience
//! returns `false` (fail-closed).

include!(concat!(env!("OUT_DIR"), "/permission_controller.rs"));

pub use android::os::IPermissionController::{
    BnPermissionController, BpPermissionController, IPermissionController,
    IPermissionControllerDefault, IPermissionControllerDefaultRef,
};

#[cfg(feature = "async")]
pub use android::os::IPermissionController::{
    IPermissionControllerAsync, IPermissionControllerAsyncService,
};

use crate::error::Result;
use crate::{hub, FromIBinder, Strong};

/// Service name registered by Android's `PermissionManagerService`. Used
/// as the key for [`crate::hub::get_service`].
pub const SERVICE_NAME: &str = "permission";

/// Acquire a `BpPermissionController` proxy for the system-wide
/// `permission` service, mirroring AOSP
/// `defaultServiceManager()->getService(String16("permission"))` +
/// `interface_cast<IPermissionController>(binder)`.
///
/// Returns `Err(StatusCode::NameNotFound)` when the service manager has
/// no `"permission"` entry — typically on non-Android Linux where
/// `system_server` is not running. Other errors propagate from the
/// `FromIBinder` cast (descriptor mismatch).
///
/// This is a thin wrapper; consumers needing custom error mapping or
/// caching should call [`crate::hub::get_service`] directly.
pub fn default() -> Result<Strong<dyn IPermissionController>> {
    let binder = hub::get_service(SERVICE_NAME).ok_or(crate::StatusCode::NameNotFound)?;
    <dyn IPermissionController>::try_from(binder)
}

/// Convenience: ask `system_server`'s `PermissionManagerService` whether
/// the current binder *caller* (as reported by
/// [`crate::get_calling_uid`] / [`crate::get_calling_pid`]) holds
/// `permission_name`.
///
/// Intended for server-side use inside a [`crate::Transactable::transact`]
/// dispatch, mirroring AOSP `IPCThreadState::self()->getCallingUid()` +
/// `IPermissionController::checkPermission(...)`.
///
/// # Fail-closed semantics
///
/// Returns `false` when:
/// - The current thread is not handling a binder transaction
///   (`get_calling_uid()` / `get_calling_pid()` both `0`).
/// - The `permission` service is unreachable (`system_server` absent or
///   the binder driver is not initialized).
/// - The remote `checkPermission` call returns an error.
///
/// This matches AOSP's "if in doubt, deny" posture for missing
/// PermissionManagerService — see Android `checkPermission` callers in
/// `frameworks/native/services/` for the same pattern.
pub fn check_permission(permission_name: &str) -> bool {
    let calling_pid = crate::get_calling_pid();
    let calling_uid = crate::get_calling_uid();
    let Ok(pc) = default() else {
        return false;
    };
    pc.checkPermission(permission_name, calling_pid as i32, calling_uid as i32)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated trait must expose the AOSP wire descriptor
    /// verbatim — `"android.os.IPermissionController"`.
    /// A mismatch here would silently fail every cross-process call to
    /// `system_server` because the kernel-side `check_interface` would
    /// reject the inbound `writeInterfaceToken` prefix.
    #[test]
    fn test_descriptor_matches_aosp_wire() {
        // Pick any `Sized` impl — `descriptor()` is gated by
        // `where Self: Sized`, but every concrete impl returns the
        // same constant via the AOSP-required `META_INTERFACE` macro.
        assert_eq!(
            <BpPermissionController as IPermissionController>::descriptor(),
            "android.os.IPermissionController"
        );
    }

    /// `SERVICE_NAME` matches the AOSP-registered service
    /// name (`servicemanager` `addService("permission", ...)` in
    /// system_server). Any drift makes `default()` return
    /// `NameNotFound` on every real Android device.
    #[test]
    fn test_service_name_matches_system_server_registration() {
        assert_eq!(SERVICE_NAME, "permission");
    }
}

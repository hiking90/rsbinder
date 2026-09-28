// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Android 16 `IServiceManager` client (typed `Service` from `getService2`/`checkService2`).
//!
//! # Typed-Service dispatch
//!
//! `dispatch_typed_service` mirrors AOSP `BackendUnifiedServiceManager::toBinderService`
//! (`BackendUnifiedServiceManager.cpp:287-313`, android-16.0.0_r4); `get_service` and
//! `check_service` both route through it:
//!
//! - `ServiceWithMetadata(swm)` with `swm.service.is_some()`: returned as-is. The fallback
//!   never shadows a service the servicemanager supplied.
//! - `ServiceWithMetadata(swm)` with `swm.service.is_none()` (the servicemanager has no entry):
//!   try the process-local fallback (AOSP `getInjectedAccessor`); on a miss return the
//!   original null `swm` so callers can still observe the metadata.
//! - `Accessor(accessor)`: run the consume-side bridge (`accessor_16::resolve_accessor`); on a
//!   miss, also try the process-local fallback.
//!
//! The process-local fallback lets a vendor process that registered a provider via
//! `add_accessor_provider` supply an `IAccessor` binder for its own instances without
//! publishing through the system servicemanager.
//!
//! AOSP's `Tag::accessor` arm (servicemanager itself returns a VINTF `<accessor>` binder) does
//! **not** consult `getInjectedAccessor`. rsbinder intentionally departs from that: a
//! peer-supplied accessor whose `getInstanceName` does not match the requested name
//! (mis-routing or impersonation) must not shadow a locally registered provider. This is a
//! hardening choice that accepts a slightly broader resolution surface.
//!
//! Without the `rpc` feature an Accessor binder cannot be consumed (only the RPC stack can):
//! `resolve_accessor_arm` logs and returns `None`, and `try_process_local_fallback` is a no-op
//! stub returning `None`, so the null-`swm` arm returns the original `swm` and callers map it
//! to `NameNotFound`.

include!(concat!(env!("OUT_DIR"), "/service_manager_16.rs"));

use crate::*;
pub use android::os::IServiceManager::{
    BnServiceManager, BpServiceManager, IServiceManager, DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PRIORITY_CRITICAL, DUMP_FLAG_PRIORITY_DEFAULT, DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL, DUMP_FLAG_PROTO, FLAG_IS_LAZY_SERVICE,
};

pub use android::os::ConnectionInfo::ConnectionInfo;
pub use android::os::IClientCallback::{BnClientCallback, IClientCallback};
pub use android::os::IServiceCallback::{BnServiceCallback, IServiceCallback};
pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

/// `Service::Accessor` arm → SWM holding a session-pinned RPC root (module doc on dispatch).
#[cfg(feature = "rpc")]
fn resolve_accessor_arm(
    name: &str,
    accessor: Option<crate::SIBinder>,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    let Some(accessor) = accessor else {
        log::warn!("Service {name} returned a null Accessor binder");
        return None;
    };
    super::accessor_16::resolve_accessor(name, accessor)
}

#[cfg(not(feature = "rpc"))]
fn resolve_accessor_arm(
    name: &str,
    _accessor: Option<crate::SIBinder>,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    log::warn!(
        "Service {name} is an Accessor but rsbinder was built without the \
         `rpc` feature; cannot bridge to RPC root"
    );
    None
}

/// Process-local provider lookup (AOSP `getInjectedAccessor`); see module doc on dispatch.
#[cfg(feature = "rpc")]
fn try_process_local_fallback(
    name: &str,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    let out = super::accessor_register::resolve_via_process_local(name)?;
    log::debug!("servicemanager_16 fallback: {name} resolved via process-local AccessorProvider");
    Some(out)
}

#[cfg(not(feature = "rpc"))]
fn try_process_local_fallback(
    _name: &str,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    None
}

/// AOSP `toBinderService` arm dispatch for a typed `Service`; module doc "Typed-Service dispatch".
fn dispatch_typed_service(
    name: &str,
    service: android::os::Service::Service,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    use android::os::Service::Service;
    match service {
        Service::ServiceWithMetadata(swm) => {
            if swm.service.is_some() {
                Some(swm)
            } else {
                try_process_local_fallback(name).or(Some(swm))
            }
        }
        Service::Accessor(accessor) => {
            resolve_accessor_arm(name, accessor).or_else(|| try_process_local_fallback(name))
        }
    }
}

/// Retrieve an existing service via a single `getService` wire call (one
/// attempt; not blocking). Use the hub's `wait_for_service` to block until the
/// service appears, or `check_service` for an explicit non-blocking lookup.
pub fn get_service(
    sm: &BpServiceManager,
    name: &str,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    match sm.getService2(name) {
        Ok(service) => dispatch_typed_service(name, service),
        Err(err) => {
            log::error!("Failed to get service {name}: {err}");
            None
        }
    }
}

/// Error-preserving variant of `get_service`: returns `Err` on a transport
/// failure instead of `None`, so a waiter can distinguish "not registered"
/// (`Ok(None)`) from "service manager unreachable" (`Err`). Mirrors AOSP
/// `realGetService`'s `Status` return.
pub fn try_get_service(
    sm: &BpServiceManager,
    name: &str,
) -> Result<Option<android::os::ServiceWithMetadata::ServiceWithMetadata>> {
    sm.getService2(name)
        .map(|service| dispatch_typed_service(name, service))
        .map_err(|e| e.into())
}

/// Retrieve an existing service called @a name from the service
/// manager. Non-blocking. Returns null if the service does not
/// exist.
pub fn check_service(
    sm: &BpServiceManager,
    name: &str,
) -> Option<android::os::ServiceWithMetadata::ServiceWithMetadata> {
    match sm.checkService2(name) {
        Ok(service) => dispatch_typed_service(name, service),
        Err(err) => {
            log::error!("Failed to check service {name}: {err}");
            None
        }
    }
}

/// Return a list of all currently running services.
pub fn list_services(sm: &BpServiceManager, dump_priority: i32) -> Vec<String> {
    match sm.listServices(dump_priority) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to list services: {err}");
            Vec::new()
        }
    }
}

pub fn add_service(
    sm: &BpServiceManager,
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    sm.addService(identifier, &binder, false, DUMP_FLAG_PRIORITY_DEFAULT)
}

/// `add_service` + `FLAG_IS_LAZY_SERVICE`, set only here as AOSP `LazyServiceRegistrar` does.
pub(crate) fn add_lazy_service(
    sm: &BpServiceManager,
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    sm.addService(
        identifier,
        &binder,
        false,
        DUMP_FLAG_PRIORITY_DEFAULT | FLAG_IS_LAZY_SERVICE,
    )
}

/// Request a callback when a service is registered.
pub fn register_for_notifications(
    sm: &BpServiceManager,
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    sm.registerForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Unregisters all requests for notifications for a specific callback.
pub fn unregister_for_notifications(
    sm: &BpServiceManager,
    name: &str,
    callback: &crate::Strong<dyn IServiceCallback>,
) -> Result<()> {
    sm.unregisterForNotifications(name, callback)
        .map_err(|e| e.into())
}

/// Register a callback for client (proxy) presence transitions on a lazy
/// service. AOSP `IServiceManager::registerClientCallback`.
pub fn register_client_callback(
    sm: &BpServiceManager,
    name: &str,
    service: &SIBinder,
    callback: &crate::Strong<dyn IClientCallback>,
) -> Result<()> {
    sm.registerClientCallback(name, service, callback)
        .map_err(|e| e.into())
}

/// Attempt to unregister a service previously registered with `add_service`.
/// AOSP `IServiceManager::tryUnregisterService`.
pub fn try_unregister_service(sm: &BpServiceManager, name: &str, service: &SIBinder) -> Result<()> {
    sm.tryUnregisterService(name, service).map_err(|e| e.into())
}

/// Every declared instance of `iface`. An interface declared as
/// `pack.age.IFoo/foo` contributes `"foo"` when asked for
/// `pack.age.IFoo`. An error is reported as "none declared", matching
/// the other lookup helpers here.
pub fn get_declared_instances(sm: &BpServiceManager, iface: &str) -> Vec<String> {
    match sm.getDeclaredInstances(iface) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to get_declared_instances({iface}): {err}");
            Vec::new()
        }
    }
}

/// Connection info declared for `name`, if any.
pub fn get_connection_info(
    sm: &BpServiceManager,
    name: &str,
) -> Option<android::os::ConnectionInfo::ConnectionInfo> {
    match sm.getConnectionInfo(name) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to get_connection_info({name}): {err}");
            None
        }
    }
}

/// Returns whether a given interface is declared on the device, even if it
/// is not started yet. For instance, this could be a service declared in the VINTF
/// manifest.
pub fn is_declared(sm: &BpServiceManager, name: &str) -> bool {
    match sm.isDeclared(name) {
        Ok(result) => result,
        Err(err) => {
            log::error!("Failed to is_declared({name}): {err}");
            false
        }
    }
}

pub fn get_interface<T: FromIBinder + ?Sized>(
    sm: &BpServiceManager,
    name: &str,
) -> Result<Strong<T>> {
    match get_service(sm, name) {
        Some(service) => match service.service {
            Some(service) => FromIBinder::try_from(service),
            None => {
                log::error!("Service {name} is not a valid IBinder");
                Err(StatusCode::NameNotFound)
            }
        },
        None => {
            log::error!("Failed to get interface {name}");
            Err(StatusCode::NameNotFound)
        }
    }
}

pub fn get_service_debug_info(
    sm: &BpServiceManager,
) -> Result<Vec<android::os::ServiceDebugInfo::ServiceDebugInfo>> {
    sm.getServiceDebugInfo().map_err(|e| e.into())
}

#[cfg(all(test, feature = "rpc"))]
mod tests {
    //! Drive `dispatch_typed_service`
    //! directly so the AOSP-faithful fallback choice (process-local
    //! provider on `ServiceWithMetadata(service: None)` AND
    //! `Accessor(None)`) is *exercised* — not just the
    //! `resolve_via_process_local` primitive it delegates to. A mutant
    //! that removes the `or_else`/`or` in [`dispatch_typed_service`]
    //! flips these tests; a fallback wired to only the `Accessor` arm
    //! is caught by
    //! `dispatch_falls_back_when_servicemanager_returns_null_service`.
    //!
    //! # Mutation gates
    //!
    //! - `register_observing_provider` sets `called` on every lookup, so a mutant removing the
    //!   `dispatch_typed_service → try_process_local_fallback` call leaves it `false`.
    //! - `dispatch_falls_back_when_servicemanager_returns_null_service` pins the AOSP-faithful
    //!   arm (`toBinderService` lines 290-313): without it the fallback never fires for the
    //!   common "no entry" case.
    //! - `dispatch_returns_servicemanager_service_unchanged_when_non_null`: dropping the
    //!   `is_some()` guard runs the fallback for a non-null entry, which sets `called`. The
    //!   descriptor assertion alone passes that mutant: the provider's dial finds no listener,
    //!   so `.or(Some(swm))` returns the original binder.
    use super::*;
    use crate::hub::accessor_register::{
        add_accessor_provider, create_accessor, AccessorAddrProvider, AccessorProviderFn,
        AccessorSockAddr,
    };
    use std::collections::HashSet;
    use std::path::PathBuf;

    fn synth_null_swm() -> android::os::Service::Service {
        android::os::Service::Service::ServiceWithMetadata(
            android::os::ServiceWithMetadata::ServiceWithMetadata {
                service: None,
                isLazyService: false,
            },
        )
    }

    /// Provider for `instance` that sets `called` on every lookup; its handle's drop unregisters.
    fn register_observing_provider(
        instance: &str,
    ) -> (
        super::super::accessor_register::AccessorProviderHandle,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::AtomicBool;
        let called = std::sync::Arc::new(AtomicBool::new(false));
        // Dialed only if the fallback runs; no listener, so the bridge yields `None`.
        let path = PathBuf::from(format!(
            "/tmp/rsb-dispatch-typed-test-{}-unused.sock",
            std::process::id()
        ));
        let want = instance.to_owned();
        let called_for_closure = std::sync::Arc::clone(&called);
        let provider: AccessorProviderFn = Box::new(move |n: &str| {
            called_for_closure.store(true, std::sync::atomic::Ordering::SeqCst);
            if n == want {
                let addr_provider: AccessorAddrProvider = Box::new({
                    let p = path.clone();
                    move |_| Ok(AccessorSockAddr::Unix(p.clone()))
                });
                Some(create_accessor(n, addr_provider))
            } else {
                None
            }
        });
        let handle = add_accessor_provider(HashSet::from([instance.to_owned()]), provider)
            .expect("registry add");
        (handle, called)
    }

    /// AOSP arm: a null-inner `ServiceWithMetadata` consults the process-local fallback.
    #[test]
    fn dispatch_falls_back_when_servicemanager_returns_null_service() {
        let instance = format!(
            "rsb.test.svcmgr16.swm-null.{}.{}",
            std::process::id(),
            line!()
        );

        // No provider yet: dispatch returns the original (null) swm.
        let out = dispatch_typed_service(&instance, synth_null_swm())
            .expect("ServiceWithMetadata is always Some, even when inner is None");
        assert!(
            out.service.is_none(),
            "no provider registered ⇒ fallback returns the original null swm \
             (so caller can still observe metadata), got {:?}",
            out.service
        );

        // Mutant gate: `called` flips iff dispatch reached `try_process_local_fallback`.
        let (_handle, called) = register_observing_provider(&instance);
        let _out = dispatch_typed_service(&instance, synth_null_swm())
            .expect("registered provider must yield a SWM");
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "dispatch_typed_service(swm.service=None) must invoke try_process_local_fallback \
             — a mutant that removes the fallback call leaves `called == false`"
        );
    }

    /// `Accessor(None)` takes the same fallback: it fires on both null arms.
    #[test]
    fn dispatch_falls_back_when_accessor_arm_binder_is_none() {
        let instance = format!(
            "rsb.test.svcmgr16.acc-null.{}.{}",
            std::process::id(),
            line!()
        );

        let null_accessor = android::os::Service::Service::Accessor(None);
        // No provider: the null Accessor arm and the fallback both yield None.
        assert!(
            dispatch_typed_service(&instance, null_accessor).is_none(),
            "Accessor(None) with no fallback provider must return None"
        );

        let (_handle, called) = register_observing_provider(&instance);
        let null_accessor = android::os::Service::Service::Accessor(None);
        // The fake path may not connect; the gate is that the provider ran, i.e. `or_else` fired.
        let _ = dispatch_typed_service(&instance, null_accessor);
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "dispatch_typed_service(Accessor(None)) must `or_else` into try_process_local_fallback \
             — a mutant that removes the `or_else` leaves `called == false`"
        );
    }

    /// A non-null `ServiceWithMetadata` round-trips unchanged; the fallback never shadows it.
    #[test]
    fn dispatch_returns_servicemanager_service_unchanged_when_non_null() {
        let instance = format!(
            "rsb.test.svcmgr16.swm-pass.{}.{}",
            std::process::id(),
            line!()
        );
        // A provider that would claim this name if the fallback ran for a non-null entry.
        let (_handle, called) = register_observing_provider(&instance);

        // A real-looking response: a local binder the assertion fingerprints by descriptor.
        let dummy: crate::SIBinder =
            crate::Interface::as_binder(&crate::Binder::new(DummyDescriptor));
        let want_desc = dummy.descriptor().to_string();
        let swm = android::os::Service::Service::ServiceWithMetadata(
            android::os::ServiceWithMetadata::ServiceWithMetadata {
                service: Some(dummy),
                isLazyService: true,
            },
        );

        let out =
            dispatch_typed_service(&instance, swm).expect("non-null inner must round-trip as-is");
        assert!(
            out.isLazyService,
            "non-null SWM must round-trip unchanged (including metadata)"
        );
        let inner = out.service.expect("non-null inner preserved");
        assert_eq!(
            inner.descriptor(),
            want_desc,
            "the dispatcher must NOT swap the servicemanager-supplied binder \
             for a process-local provider"
        );
        assert!(
            !called.load(std::sync::atomic::Ordering::SeqCst),
            "a non-null entry must not consult the process-local providers \
             (mutant: drop the `is_some()` guard ⇒ `called == true`)"
        );
    }

    /// Minimal `Remotable` whose descriptor fingerprints the non-null arm's binder.
    struct DummyDescriptor;
    impl crate::Interface for DummyDescriptor {}
    impl crate::Remotable for DummyDescriptor {
        fn descriptor() -> &'static str {
            "rsb.test.svcmgr16.dummy"
        }
        fn on_transact(
            &self,
            _code: crate::TransactionCode,
            _reader: &mut crate::Parcel,
            _reply: &mut crate::Parcel,
        ) -> crate::error::Result<()> {
            Err(crate::error::StatusCode::UnknownTransaction)
        }
        fn on_dump(&self, _w: &mut dyn std::io::Write, _a: &[String]) -> crate::error::Result<()> {
            Ok(())
        }
    }
}

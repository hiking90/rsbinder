// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

include!(concat!(env!("OUT_DIR"), "/service_manager_16.rs"));

use crate::*;
pub use android::os::IServiceManager::{
    BnServiceManager, BpServiceManager, IServiceManager, DUMP_FLAG_PRIORITY_ALL,
    DUMP_FLAG_PRIORITY_CRITICAL, DUMP_FLAG_PRIORITY_DEFAULT, DUMP_FLAG_PRIORITY_HIGH,
    DUMP_FLAG_PRIORITY_NORMAL, DUMP_FLAG_PROTO, FLAG_IS_LAZY_SERVICE,
};

pub use android::os::IServiceCallback::{BnServiceCallback, IServiceCallback};
pub use android::os::ServiceDebugInfo::ServiceDebugInfo;

/// Bridge the `Service::Accessor` arm of
/// `getService2`/`checkService2` into a `ServiceWithMetadata` whose
/// `service` is the RPC root pinned by an owning [`RpcSession`].
///
/// Optional Accessor arm: an Accessor binder is **only** consumable via
/// the RPC stack, so a build without the `rpc` feature falls back to
/// the historical "log + None" behavior — byte-unchanged for that
/// build.
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

/// Process-local AccessorProvider fallback. AOSP
/// `BackendUnifiedServiceManager::toBinderService`
/// (`BackendUnifiedServiceManager.cpp:287-313`, android-16.0.0_r4)
/// calls `getInjectedAccessor(name, &accessor)` from the
/// `Tag::serviceWithMetadata` arm when `serviceWithMetadata.service ==
/// nullptr` — i.e. when the kernel servicemanager has no entry for
/// `name`. A vendor process that registered a provider via
/// [`add_accessor_provider`](super::accessor_register::add_accessor_provider)
/// can then supply an `IAccessor` binder for *its* instances *without*
/// publishing through the system servicemanager.
///
/// AOSP's `Tag::accessor` arm is a *separate* path (for the rare case
/// where servicemanager itself returns a VINTF `<accessor>` binder)
/// and **does not** consult `getInjectedAccessor`.
///
/// rsbinder *intentionally departs* from that AOSP policy: the
/// `Accessor` arm here also calls `try_process_local_fallback` on miss.
/// The rationale is defensive — a peer-supplied accessor whose
/// `getInstanceName` doesn't match the requested name (mis-routing or
/// impersonation) shouldn't shadow a locally-registered provider. This
/// is a hardening choice, not an AOSP-faithfulness defect: rsbinder
/// accepts a slightly broader resolution surface in exchange for
/// closing a class of silent failures.
///
/// The fallback fires when (a) the `ServiceWithMetadata` arm's inner
/// binder is `None` (the common "no entry" signal) or (b) the
/// `Accessor` arm fails to resolve. The bridge from the returned
/// `IAccessor` SIBinder to an `RpcSession` root is the same helper
/// [`super::accessor_16::resolve_accessor`].
///
/// `try_process_local_fallback` carries a `cfg(feature = "rpc")` gate,
/// but `dispatch_typed_service` itself is cfg-free. `rpc`-OFF builds
/// reach the `swm.service.is_none()` arm and call the **no-op stub**,
/// which returns `None`, so the `or(Some(swm))` falls back to the
/// original null `swm` — the observable result is unchanged (callers
/// map it to `NameNotFound` identically); only an extra no-op stub
/// frame is invoked.
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

/// AOSP-faithful `getService2` → typed-Service dispatch. Folds the
/// `Tag::serviceWithMetadata` and `Tag::accessor` arms into one place
/// (both `get_service` and `check_service` route through this).
///
/// Mirrors `BackendUnifiedServiceManager::toBinderService` arm
/// dispatch:
///   * `ServiceWithMetadata(swm)` with `swm.service.is_some()` ⇒
///     return as-is.
///   * `ServiceWithMetadata(swm)` with `swm.service.is_none()` ⇒ try
///     the process-local fallback (AOSP `getInjectedAccessor`);
///     return the original `swm` (null inside) on miss so callers
///     can still observe the metadata.
///   * `Accessor(accessor)` ⇒ run the consume-side bridge; on miss,
///     also try the process-local fallback (defensive — a mis-routed
///     accessor shouldn't shadow a registered provider).
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

/// Retrieve an existing service, blocking for a few seconds if it doesn't yet
/// exist.
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
    //! flips these tests; a regression that wires the fallback to
    //! only one arm (the pre-fix bug) is caught by
    //! `dispatch_falls_back_when_servicemanager_returns_null_service`.
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

    /// Bake a dummy provider into the global registry under a unique
    /// instance name. The returned handle's `Drop` un-registers — keep
    /// it bound until the assertions complete.
    fn register_dummy_provider(
        instance: &str,
    ) -> crate::hub::accessor_register::AccessorProviderHandle {
        // The provider's `addConnection` would dial `path`, which we
        // never have to actually accept on — these tests only check the
        // *dispatch routing*, not the bridge round-trip. (The full
        // bridge round-trip is covered by `rpc_accessor.rs`.) An
        // unreachable path is fine because `dispatch_typed_service`
        // surfaces the registered Accessor binder without yet calling
        // `addConnection`.
        let path = PathBuf::from(format!(
            "/tmp/rsb-dispatch-typed-test-{}-unused.sock",
            std::process::id()
        ));
        let want = instance.to_owned();
        let provider: AccessorProviderFn = Box::new(move |n: &str| {
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
        add_accessor_provider(HashSet::from([instance.to_owned()]), provider).expect("registry add")
    }

    /// Wrap the provider closure with an
    /// observable side effect so a mutant removing the
    /// `dispatch_typed_service → try_process_local_fallback` call
    /// flips the test red. Returns the `(handle, called)` pair —
    /// `called.load(SeqCst)` asserts the dispatcher actually reached
    /// the process-local fallback registry.
    fn register_observing_provider(
        instance: &str,
    ) -> (
        super::super::accessor_register::AccessorProviderHandle,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
    ) {
        use std::sync::atomic::AtomicBool;
        let called = std::sync::Arc::new(AtomicBool::new(false));
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

    /// **AOSP-faithful arm**: `Tag::serviceWithMetadata` with null
    /// inner ⇒ `dispatch_typed_service` MUST consult the process-local
    /// fallback (matches `BackendUnifiedServiceManager::toBinderService`
    /// lines 290-313). Pre-fix, this was wired to the wrong arm and
    /// the fallback never fired for the common "no entry" case; this
    /// test pins the corrected routing.
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

        // Now register a provider and re-dispatch — the fallback must
        // surface the registered Accessor's resolved root.
        // Mutant gate: `called` flips iff the dispatcher
        // actually walked into `try_process_local_fallback` →
        // registry lookup → our provider closure.
        let (_handle, called) = register_observing_provider(&instance);
        let _out = dispatch_typed_service(&instance, synth_null_swm())
            .expect("registered provider must yield a SWM");
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "dispatch_typed_service(swm.service=None) must invoke try_process_local_fallback \
             — a mutant that removes the fallback call leaves `called == false`"
        );
    }

    /// `Tag::accessor` with a null inner binder ⇒ same fallback path
    /// (this was rsbinder's original wiring; preserved here so the
    /// pre-fix path stays covered).
    #[test]
    fn dispatch_falls_back_when_accessor_arm_binder_is_none() {
        let instance = format!(
            "rsb.test.svcmgr16.acc-null.{}.{}",
            std::process::id(),
            line!()
        );

        let null_accessor = android::os::Service::Service::Accessor(None);
        // No provider ⇒ both arms (Accessor with null + fallback) return
        // None ⇒ `dispatch_typed_service` returns None.
        assert!(
            dispatch_typed_service(&instance, null_accessor).is_none(),
            "Accessor(None) with no fallback provider must return None"
        );

        let (_handle, called) = register_observing_provider(&instance);
        let null_accessor = android::os::Service::Service::Accessor(None);
        // With a registered provider, dispatch_typed_service tries the
        // Accessor arm first (null ⇒ resolve_accessor_arm returns None),
        // then falls back. The fallback's full bridge may not connect
        // (fake path) — the mutant gate below is that the
        // provider closure was invoked, proving the `or_else` fired.
        let _ = dispatch_typed_service(&instance, null_accessor);
        assert!(
            called.load(std::sync::atomic::Ordering::SeqCst),
            "dispatch_typed_service(Accessor(None)) must `or_else` into try_process_local_fallback \
             — a mutant that removes the `or_else` leaves `called == false`"
        );
    }

    /// `ServiceWithMetadata` with a non-null inner binder MUST be
    /// returned unchanged — the fallback must NEVER shadow a real
    /// servicemanager-supplied service (would silently swap legitimate
    /// services for process-local providers under name collisions).
    #[test]
    fn dispatch_returns_servicemanager_service_unchanged_when_non_null() {
        let instance = format!(
            "rsb.test.svcmgr16.swm-pass.{}.{}",
            std::process::id(),
            line!()
        );
        // Register a provider that WOULD claim this name if the
        // dispatcher consulted the fallback for a non-null entry.
        let _handle = register_dummy_provider(&instance);

        // Synthesize a "real servicemanager response" — non-null inner.
        // Use a dummy local binder so the assertion can fingerprint it
        // (`SIBinder::descriptor()` round-trip).
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
             for a process-local provider (mutant: drop the `is_some()` guard \
             ⇒ this assertion fails)"
        );
    }

    /// Minimal `Remotable` for the dispatcher tests — only needed for
    /// the descriptor round-trip in the non-null arm.
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

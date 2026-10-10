// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Native service implementation utilities.
//!
//! This module provides helper types and functions for implementing binder services
//! on the server side, including the `Binder` wrapper for native service objects
//! and transaction handling utilities.
//!
//! # Local-binder cast
//!
//! `Binder::<B>::try_from(SIBinder)` recovers the local binder behind a
//! type-erased handle (the Rust counterpart of C++ `IBinder::localBinder`) by
//! upcasting a clone of the `Arc<dyn IBinder>` to `Arc<dyn Any + Send + Sync>`
//! and calling `Arc::downcast::<Inner<B>>`. The `TypeId` it compares comes
//! from the trait object's vtable, i.e. the allocation's concrete type, never
//! from `IBinder::as_any`: an external `IBinder` impl that delegates
//! `as_any()` and `descriptor()` to a wrapped `Binder<B>` fails the cast with
//! `BadValue` instead of being reinterpreted as `Inner<B>`.
//!
//! # Flat-binder flags
//!
//! `BinderFeatures::flat_flags` encodes the `flat_binder_object.flags` word.
//! `FLAT_BINDER_FLAG_ACCEPTS_FDS` is always set: rsbinder accepts file
//! descriptors unconditionally in its native binder protocol.
//! `FLAT_BINDER_FLAG_TXN_SECURITY_CTX` is set for `set_requesting_sid` only
//! where `process_state::selinux_available` holds. Bit layout
//! (cross-checked against `kernel/include/uapi/linux/android/binder.h`):
//!
//! ```text
//! bit 0-7   (0xff):   FLAT_BINDER_FLAG_PRIORITY_MASK
//! bit 8     (0x100):  FLAT_BINDER_FLAG_ACCEPTS_FDS
//! bit 9-10  (0x600):  scheduler policy (SHIFT = 9, VALUE_MASK = 0x3)
//! bit 11    (0x800):  FLAT_BINDER_FLAG_INHERIT_RT
//! bit 12    (0x1000): FLAT_BINDER_FLAG_TXN_SECURITY_CTX
//! ```
//!
//! An out-of-range `min_priority` is masked to the low byte and
//! `min_sched_policy` to two bits, so neither bleeds into the adjacent
//! scheduler-policy or `INHERIT_RT` field and changes the scheduler class of
//! every transaction.
//!
//! # Runtime stability
//!
//! `Inner` stores its `Stability` as an `AtomicU8` tag (`STABILITY_TAG_*`),
//! so the runtime setters (`mark_vintf`, `force_downgrade_to_system_stability`,
//! `force_downgrade_to_vendor_stability`) mutate it without a lock while the
//! `flat_binder_object` emit path reads it. `Relaxed` is enough for the tag:
//! the `parceled` flag (Acquire/Release) provides the only happens-before the
//! setters depend on.
//!
//! `parceled` is AOSP `BBinder::mParceled`. It becomes `true` the first time
//! the binder is written to a parcel; after that the setters refuse with
//! `InvalidOperation`. AOSP aborts at the same point via
//! `LOG_ALWAYS_FATAL_IF(mParceled, ...)`
//! ([`Binder.cpp:579,606,659,678,713`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=579));
//! rsbinder returns an error so the refusal composes with the `Result`-based
//! AIDL surface.
//!
//! # Attached objects
//!
//! AOSP `BBinder::Extras::mObjectMgr` (Binder.cpp:523) keys attachments by
//! `const void*` identity; rsbinder keys them by `TypeId`, which avoids the
//! provenance / ABA hazard of raw-pointer identity. Trade-off: one attached
//! object per Rust type per binder; callers that need several objects of one
//! concrete type wrap them in distinct newtypes. The map starts empty (no
//! allocation until the first `attach_object`), so the per-binder cost is the
//! unlocked `Mutex` header plus an empty `HashMap` header (~56 bytes on
//! 64-bit).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::{
    binder::*, error::*, parcel::*, parcelable::SerializeOption, ref_counter::RefCounter,
    thread_state,
};

/// Opt-in flags requested at native-binder construction time.
///
/// Each flag triggers a kernel-side behavior that has a non-zero cost,
/// so callers explicitly opt in only what they need. The struct is
/// `#[non_exhaustive]` to allow new flags to be added without a SemVer
/// break. From outside this crate, construct it by starting from
/// [`BinderFeatures::default`] and assigning the flags you want:
///
/// ```
/// use rsbinder::BinderFeatures;
/// let mut features = BinderFeatures::default();
/// features.set_requesting_sid = true;
/// ```
///
/// ```compile_fail
/// use rsbinder::BinderFeatures;
/// // E0639 — both struct-literal forms are blocked from outside the
/// // defining crate, with or without functional update syntax.
/// let _ = BinderFeatures { set_requesting_sid: true };
/// let _ = BinderFeatures { set_requesting_sid: true, ..Default::default() };
/// ```
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub struct BinderFeatures {
    /// Request that the kernel attach the caller's SELinux security
    /// context to every transaction targeting this binder. When set,
    /// the kernel dispatches via `BR_TRANSACTION_SEC_CTX` and the
    /// transaction handler can read the caller's context through
    /// [`crate::thread_state::CallingContext::default`]`.sid`.
    ///
    /// Honoured only where SELinux is available: on Android, or on a Linux
    /// host where `/sys/fs/selinux/enforce` exists — the same check that
    /// decides whether [`get_calling_sid`](crate::thread_state::get_calling_sid)
    /// returns the context. Elsewhere the binder is published without
    /// `FLAT_BINDER_FLAG_TXN_SECURITY_CTX` and the calling sid is `None`.
    /// The driver fails a transaction to a binder that requests the context
    /// when no LSM can produce one (`BR_FAILED_REPLY`, android17-6.18
    /// `binder_transaction`), and only an SELinux context ends in a NUL
    /// rsbinder can find its end by, so sending the flag elsewhere would
    /// only fail calls or yield a context rsbinder discards.
    ///
    /// Default: `false`. Has a per-transaction cost (kernel must
    /// serialize `secctx`), so opt in only on services that perform
    /// SELinux-domain-based authorization.
    pub set_requesting_sid: bool,

    /// Advertise a minimum scheduler policy on the
    /// binder node. Must be one of `SCHED_NORMAL`/`SCHED_BATCH` (low /
    /// best-effort) or `SCHED_FIFO`/`SCHED_RR` (real-time); kernel
    /// `binder.c::binder_priority_to_native` clamps anything else.
    /// Pair with [`Self::min_priority`] — both fields are encoded into
    /// `flat_binder_object.flags` so the driver can lift an incoming
    /// transaction's worker thread to the requested floor before
    /// running the handler.
    ///
    /// Default: `None` (kernel keeps the caller's policy). Acts as a
    /// floor: the driver applies it after [`Self::inherit_rt`] has
    /// decided whether an RT caller keeps its policy.
    pub min_sched_policy: Option<i32>,

    /// Minimum priority within the policy declared
    /// by [`Self::min_sched_policy`]. For SCHED_FIFO/SCHED_RR this is
    /// the kernel RT priority (`1..=99`); for SCHED_NORMAL/BATCH it is
    /// a nice-bias value. Only the low 8 bits are used (mask
    /// `FLAT_BINDER_FLAG_PRIORITY_MASK = 0xff`); higher bits are masked
    /// off at encode time, so a value outside the expected range is
    /// silently truncated — the kernel will then interpret whatever low
    /// 8 bits remain, which may not be the priority the caller
    /// intended. Callers should validate against the policy's allowed
    /// range before constructing `BinderFeatures` (AOSP
    /// `BBinder::setMinSchedulerPolicy` aborts on out-of-range input).
    pub min_priority: Option<i32>,

    /// Set the kernel
    /// `FLAT_BINDER_FLAG_INHERIT_RT` bit (`0x800`). When `true` and the
    /// caller is running under SCHED_FIFO/SCHED_RR, the binder driver
    /// lifts the worker thread to the *caller's* RT priority for the
    /// transaction duration. Required for audio/camera HAL latency
    /// guarantees.
    ///
    /// Default: `false`. Independent of `min_sched_policy`: without it the
    /// driver demotes an RT caller to `SCHED_NORMAL` before applying the
    /// node's minimum (`binder.c::binder_transaction_priority`).
    pub inherit_rt: bool,
}

impl BinderFeatures {
    /// Encodes the `flat_binder_object.flags` word; bit layout in module doc "Flat-binder flags".
    pub(crate) fn flat_flags(self) -> u32 {
        self.flat_flags_with(crate::process_state::selinux_available())
    }

    /// [`Self::flat_flags`]; `TXN_SECURITY_CTX` only with `selinux` (see `set_requesting_sid`).
    fn flat_flags_with(self, selinux: bool) -> u32 {
        let mut f = crate::sys::FLAT_BINDER_FLAG_ACCEPTS_FDS;
        if self.set_requesting_sid && selinux {
            f |= crate::sys::FLAT_BINDER_FLAG_TXN_SECURITY_CTX;
        }
        if let Some(priority) = self.min_priority {
            f |= (priority as u32) & crate::sys::FLAT_BINDER_FLAG_PRIORITY_MASK;
        }
        if let Some(policy) = self.min_sched_policy {
            // Only 2 policy bits fit; higher bits would corrupt the adjacent INHERIT_RT bit.
            f |= ((policy as u32) & 0x3) << 9;
        }
        if self.inherit_rt {
            f |= crate::sys::FLAT_BINDER_FLAG_INHERIT_RT;
        }
        f
    }
}

/// `Stability` tags for `Inner::stability`; see module doc "Runtime stability".
const STABILITY_TAG_LOCAL: u8 = 0;
const STABILITY_TAG_VENDOR: u8 = 1;
const STABILITY_TAG_SYSTEM: u8 = 2;
const STABILITY_TAG_VINTF: u8 = 3;

fn stability_to_tag(s: Stability) -> u8 {
    match s {
        Stability::Local => STABILITY_TAG_LOCAL,
        Stability::Vendor => STABILITY_TAG_VENDOR,
        Stability::System => STABILITY_TAG_SYSTEM,
        Stability::Vintf => STABILITY_TAG_VINTF,
    }
}

fn stability_from_tag(tag: u8) -> Stability {
    match tag {
        STABILITY_TAG_LOCAL => Stability::Local,
        STABILITY_TAG_VENDOR => Stability::Vendor,
        STABILITY_TAG_SYSTEM => Stability::System,
        STABILITY_TAG_VINTF => Stability::Vintf,
        // Only `stability_to_tag` stores (0..=3): abort, never emit a mis-stamped flat object.
        other => unreachable!("invalid stability tag {other}: only 0..=3 are stored"),
    }
}

// Crate-private, never embedded by value: `try_from`'s Arc cast relies on it (see module doc).
struct Inner<T: Remotable + Send + Sync> {
    remotable: T,
    /// `STABILITY_TAG_*` value, `Relaxed`; see module doc "Runtime stability".
    stability: AtomicU8,
    /// AOSP `BBinder::mParceled`: set on the first parcel write, then the stability setters refuse.
    parceled: AtomicBool,
    binder_flags: u32,
    strong: RefCounter,
    weak: RefCounter,
    extension: RwLock<Option<SIBinder>>,
    /// Attached objects, one per `TypeId`; see module doc "Attached objects".
    objects: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl<T: Remotable> Inner<T> {
    /// `InvalidOperation` once `parceled` is set; AOSP aborts on `mParceled` (Binder.cpp:579).
    fn set_stability_guarded(&self, level: Stability) -> Result<()> {
        if self.parceled.load(Ordering::Acquire) {
            return Err(StatusCode::InvalidOperation);
        }
        self.stability
            .store(stability_to_tag(level), Ordering::Relaxed);
        Ok(())
    }

    // The following functions can be redefined depending on the service.
    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            INTERFACE_TRANSACTION => reply.write(T::descriptor()),
            DUMP_TRANSACTION => {
                // AOSP `readFileDescriptor`, kernel or RPC; a kernel parcel closes its own fd.
                let mut file = File::from(crate::file_descriptor::read_raw_fd(_reader)?);

                let argc = _reader.read::<i32>()?;
                let mut argv = Vec::new();
                for _ in 0..argc {
                    argv.push(_reader.read::<String>()?);
                }

                self.remotable.on_dump(&mut file, argv.as_slice())
            }
            SHELL_COMMAND_TRANSACTION => {
                log::error!("SHELL_COMMAND_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }
            SYSPROPS_TRANSACTION => {
                log::error!("SYSPROPS_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
}

impl<T: 'static + Remotable> IBinder for Inner<T> {
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        Ok(self
            .extension
            .read()
            .expect("Extension lock poisoned")
            .clone())
    }

    fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        let mut ext = self.extension.write().expect("Extension lock poisoned");
        *ext = Some(extension.clone());
        Ok(())
    }

    fn link_to_death(&self, _recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        log::error!("Binder<T> does not support link_to_death.");
        Err(StatusCode::InvalidOperation)
    }

    /// Always `InvalidOperation`: a local binder has no death notification to remove.
    fn unlink_to_death(&self, _recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        log::error!("Binder<T> does not support unlink_to_death.");
        Err(StatusCode::InvalidOperation)
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        Ok(())
    }

    fn stability(&self) -> Stability {
        stability_from_tag(self.stability.load(Ordering::Relaxed))
    }

    fn local_binder_flags(&self) -> u32 {
        self.binder_flags
    }

    fn force_downgrade_to_system_stability(&self) -> Result<()> {
        self.set_stability_guarded(Stability::System)
    }

    fn force_downgrade_to_vendor_stability(&self) -> Result<()> {
        self.set_stability_guarded(Stability::Vendor)
    }

    fn mark_vintf(&self) -> Result<()> {
        self.set_stability_guarded(Stability::Vintf)
    }

    fn was_parceled(&self) -> bool {
        self.parceled.load(Ordering::Acquire)
    }

    fn set_parceled(&self) {
        self.parceled.store(true, Ordering::Release);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        Some(self)
    }

    /// RPC dispatch: `Inner::transact` minus `check_interface` and the position reset (adapter's).
    #[cfg(feature = "rpc")]
    fn rpc_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            PING_TRANSACTION => Ok(()),
            EXTENSION_TRANSACTION => {
                let ext = self.extension.read().expect("Extension lock poisoned");
                SerializeOption::serialize_option(ext.as_ref(), reply)?;
                Ok(())
            }
            STOP_RECORDING_TRANSACTION | START_RECORDING_TRANSACTION => {
                log::error!("recording transactions are not supported over RPC");
                Err(StatusCode::InvalidOperation)
            }
            DEBUG_PID_TRANSACTION => {
                reply.write::<i32>(&rustix::process::getpid().as_raw_nonzero().get())
            }
            _ => match self.remotable.on_transact(code, reader, reply) {
                Ok(_) => Ok(()),
                Err(StatusCode::UnknownTransaction) => {
                    // As `Inner::transact`: fall back for INTERFACE_TRANSACTION etc.
                    self.on_transact(code, reader, reply)
                }
                Err(err) => Err(err),
            },
        }
    }

    fn descriptor(&self) -> &str {
        T::descriptor()
    }

    fn is_remote(&self) -> bool {
        false
    }

    fn inc_strong(&self, _strong: &SIBinder) -> Result<()> {
        self.strong.inc(|| Ok(()))
    }

    fn attempt_inc_strong(&self) -> bool {
        self.strong.attempt_inc(true, || true, || {})
    }

    fn dec_strong(&self, strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        self.strong.dec(|| {
            if let Some(strong) = strong {
                let _ = ManuallyDrop::into_inner(strong);
            }
            Ok(())
        })
    }

    fn inc_weak(&self, _weak: &WIBinder) -> Result<()> {
        self.weak.inc(|| Ok(()))
    }

    fn dec_weak(&self) -> Result<()> {
        self.weak.dec(|| Ok(()))
    }
}

impl<T: Remotable> Transactable for Inner<T> {
    fn transaction_name(&self, code: TransactionCode) -> Option<&'static str> {
        T::transaction_name(code)
    }

    fn transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        reader.set_data_position(0);
        match code {
            PING_TRANSACTION => {
                // Noting to do for PING_TRANSACTION.
                Ok(())
            }
            EXTENSION_TRANSACTION => {
                let ext = self.extension.read().expect("Extension lock poisoned");
                SerializeOption::serialize_option(ext.as_ref(), reply)?;
                Ok(())
            }

            STOP_RECORDING_TRANSACTION => {
                log::error!("STOP_RECORDING_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }

            START_RECORDING_TRANSACTION => {
                log::error!("START_RECORDING_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }

            DEBUG_PID_TRANSACTION => {
                reply.write::<i32>(&rustix::process::getpid().as_raw_nonzero().get())
            }

            _ => {
                if (FIRST_CALL_TRANSACTION..=LAST_CALL_TRANSACTION).contains(&code)
                    && !(thread_state::check_interface(reader, T::descriptor())?)
                {
                    // Status, never payload (read as success); as AOSP NDK `ABBinder::onTransact`.
                    return Err(StatusCode::BadType);
                }

                match self.remotable.on_transact(code, reader, reply) {
                    Ok(_) => Ok(()),
                    Err(err) => {
                        if err == StatusCode::UnknownTransaction {
                            self.on_transact(code, reader, reply)
                        } else {
                            Err(err)
                        }
                    }
                }
            }
        }
    }
}

/// A Binder object that wraps a service implementation for IPC.
///
/// `Binder<T>` provides a wrapper around a service implementation that implements
/// the `Remotable` trait, handling the low-level binder protocol details and
/// dispatching incoming transactions to the appropriate service methods.
pub struct Binder<T: 'static + Remotable + Send + Sync> {
    inner: Arc<Inner<T>>,
}

impl<T: 'static + Remotable> Binder<T> {
    /// Create a new `Binder<T>` with default stability and no opt-in features.
    ///
    /// Equivalent to `new_with_stability_and_features(remotable,
    /// Stability::default(), BinderFeatures::default())`. Use this for the
    /// common case where the AIDL generator (or the caller) does not need to
    /// override stability or request kernel-side features such as
    /// [`BinderFeatures::set_requesting_sid`].
    pub fn new(remotable: T) -> Self {
        Self::new_with_stability_and_features(remotable, Default::default(), Default::default())
    }

    /// Create a new `Binder<T>` with default stability and a custom feature set.
    ///
    /// See [`BinderFeatures`] for the available opt-ins.
    pub fn new_with_features(remotable: T, features: BinderFeatures) -> Self {
        Self::new_with_stability_and_features(remotable, Default::default(), features)
    }

    /// Create a new `Binder<T>` with an explicit stability level and default
    /// features.
    ///
    /// Stability is normally set by the AIDL generator via `@VintfStability`,
    /// not by user-side construction. Reach for this only when constructing
    /// `Binder<T>` directly without going through the generator.
    pub fn new_with_stability(remotable: T, stability: Stability) -> Self {
        Self::new_with_stability_and_features(remotable, stability, Default::default())
    }

    /// Create a new `Binder<T>` with explicit stability and feature set.
    ///
    /// This is the underlying constructor; the other `new_*` variants
    /// delegate to this with default values for the parameters they
    /// don't take. See [`BinderFeatures`] for the available feature opt-ins.
    pub fn new_with_stability_and_features(
        remotable: T,
        stability: Stability,
        features: BinderFeatures,
    ) -> Self {
        Binder::<T> {
            inner: Arc::new(Inner {
                remotable,
                stability: AtomicU8::new(stability_to_tag(stability)),
                parceled: AtomicBool::new(false),
                binder_flags: features.flat_flags(),
                strong: Default::default(),
                weak: Default::default(),
                extension: RwLock::new(None),
                objects: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Attach a typed object to this binder.
    ///
    /// AOSP `BBinder::attachObject(objectID, object, cleanupCookie, func)`
    /// ([Binder.cpp:523](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=523))
    /// equivalent, with two intentional deviations from the C++ surface:
    ///
    /// 1. **Key by [`std::any::TypeId`], not raw pointer.** AOSP keys
    ///    by `const void*` address identity, which trips the LLVM
    ///    provenance / ABA hazard when the address is reused after
    ///    free. Keying by `TypeId` is safe at the cost of "one
    ///    attachment per Rust type"; callers needing multiple should
    ///    wrap distinct newtypes.
    /// 2. **No cleanup callback.** `Arc<O>` drop runs on `detach_object`
    ///    or `Binder` drop, replacing AOSP's `object_cleanup_func`.
    ///
    /// Returns the previously-attached object of the same type (if any),
    /// matching AOSP's "return old, new entry replaces" contract.
    pub fn attach_object<O: Any + Send + Sync>(
        &self,
        object: Arc<O>,
    ) -> Option<Arc<dyn Any + Send + Sync>> {
        let mut map = self.inner.objects.lock().expect("objects lock poisoned");
        map.insert(TypeId::of::<O>(), object)
    }

    /// Find a previously attached object of type `O`.
    /// AOSP [`BBinder::findObject`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=532)
    /// equivalent. Returns `None` iff no object of this type is attached;
    /// downcast back to `O` cannot fail because the entry was keyed by
    /// `TypeId::of::<O>()` at insertion.
    pub fn find_object<O: Any + Send + Sync>(&self) -> Option<Arc<O>> {
        let map = self.inner.objects.lock().expect("objects lock poisoned");
        map.get(&TypeId::of::<O>()).cloned().map(|arc| {
            arc.downcast::<O>()
                .expect("TypeId-keyed entry must downcast back to its insertion type")
        })
    }

    /// Remove and return the attached object of type
    /// `O`. AOSP [`BBinder::detachObject`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=541)
    /// equivalent.
    pub fn detach_object<O: Any + Send + Sync>(&self) -> Option<Arc<O>> {
        let mut map = self.inner.objects.lock().expect("objects lock poisoned");
        map.remove(&TypeId::of::<O>()).map(|arc| {
            arc.downcast::<O>()
                .expect("TypeId-keyed entry must downcast back to its insertion type")
        })
    }
}

impl<T: 'static + Remotable> Binder<T> {
    /// Set the extension binder object.
    pub fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        self.inner.set_extension(extension)
    }

    /// Return the extension binder object, if set.
    pub fn get_extension(&self) -> Result<Option<SIBinder>> {
        self.inner.get_extension()
    }
}

impl<T: 'static + Remotable> Interface for Binder<T> {
    fn as_binder(&self) -> SIBinder {
        SIBinder::new(self.inner.clone()).unwrap_or_else(|e| {
            panic!(
                "Failed to create SIBinder for {}. StatusCode({:?})",
                T::descriptor(),
                e
            )
        })
    }
}

impl<T: Remotable> Clone for Binder<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: 'static + Remotable> Deref for Binder<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner.remotable
    }
}

// Rust counterpart of C++ `IBinder::localBinder`; soundness: see module doc.
impl<B: Remotable + 'static> TryFrom<SIBinder> for Binder<B> {
    type Error = StatusCode;

    fn try_from(ibinder: SIBinder) -> Result<Self> {
        if B::descriptor() != ibinder.descriptor() {
            // Single funnel of every failed interface cast: the one place BadType is explained.
            log::error!(
                "binder interface cast mismatch: expected `{}`, got `{}`",
                B::descriptor(),
                ibinder.descriptor()
            );
            return Err(StatusCode::BadType);
        }

        // Two bindings: upcasting `Arc::clone`'s result in one expression fails to infer (E0308).
        let arc: Arc<dyn IBinder> = Arc::clone(ibinder.as_arc());
        let any: Arc<dyn Any + Send + Sync> = arc;
        match any.downcast::<Inner<B>>() {
            Ok(inner) => Ok(Self { inner }),
            Err(_) => {
                // Descriptor matched: a remote proxy, another `Remotable`, or a delegating wrapper.
                log::error!(
                    "cast to local Binder<{}> failed: not a local binder (remote proxy or different Remotable)",
                    B::descriptor()
                );
                Err(StatusCode::BadValue)
            }
        }
    }
}

/// Determine whether the current thread is currently executing an incoming
/// transaction.
pub fn is_handling_transaction() -> bool {
    thread_state::is_handling_transaction()
}

#[cfg(test)]
mod feature_flags_tests {
    use super::*;
    use crate::sys::{FLAT_BINDER_FLAG_ACCEPTS_FDS, FLAT_BINDER_FLAG_TXN_SECURITY_CTX};

    struct DummyRemotable;
    impl crate::Remotable for DummyRemotable {
        fn descriptor() -> &'static str
        where
            Self: Sized,
        {
            "test.dummy"
        }
        fn on_transact(
            &self,
            _: crate::TransactionCode,
            _: &mut crate::Parcel,
            _: &mut crate::Parcel,
        ) -> crate::Result<()> {
            Ok(())
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> crate::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_features_set_only_accepts_fds() {
        let b = Binder::new(DummyRemotable);
        let flags = b.inner.local_binder_flags();
        assert_eq!(flags, FLAT_BINDER_FLAG_ACCEPTS_FDS);
        assert_eq!(flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX, 0);
    }

    /// `TXN_SECURITY_CTX` is requested only where SELinux is available.
    #[test]
    fn requesting_sid_sets_txn_security_ctx_only_with_selinux() {
        let features = BinderFeatures {
            set_requesting_sid: true,
            ..Default::default()
        };
        let with = features.flat_flags_with(true);
        assert_ne!(with & FLAT_BINDER_FLAG_TXN_SECURITY_CTX, 0);
        assert_ne!(with & FLAT_BINDER_FLAG_ACCEPTS_FDS, 0);
        let without = features.flat_flags_with(false);
        assert_eq!(
            without, FLAT_BINDER_FLAG_ACCEPTS_FDS,
            "no SELinux, no secctx request"
        );

        let b = Binder::new_with_features(DummyRemotable, features);
        let flags = b.inner.local_binder_flags();
        assert_eq!(
            flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX != 0,
            crate::process_state::selinux_available()
        );
    }

    // BinderFeatures sched policy / priority / inherit_rt encoding into flat_binder_object.flags.

    use crate::sys::{
        FLAT_BINDER_FLAG_INHERIT_RT, FLAT_BINDER_FLAG_PRIORITY_MASK,
        FLAT_BINDER_FLAG_SCHED_POLICY_MASK,
    };

    /// The AOSP constant must exist at its kernel-defined value (`0x800`).
    #[test]
    fn flat_binder_flag_inherit_rt_matches_kernel_uapi() {
        assert_eq!(FLAT_BINDER_FLAG_INHERIT_RT, 0x800);
    }

    /// The post-shift named mask (`0x600`) is what AOSP libbinder uses.
    #[test]
    fn flat_binder_flag_sched_policy_mask_matches_kernel_uapi() {
        assert_eq!(FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0x600);
    }

    /// Default `BinderFeatures` leaves the INHERIT_RT, SCHED_POLICY and priority bits unset.
    #[test]
    fn default_features_zero_rt_bits() {
        let b = Binder::new(DummyRemotable);
        let flags = b.inner.local_binder_flags();
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0);
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0);
        assert_eq!(flags & FLAT_BINDER_FLAG_PRIORITY_MASK, 0);
    }

    /// SCHED_FIFO (1) with priority 42 encodes as `1 << 9 | 42 = 0x22A`, plus ACCEPTS_FDS.
    #[test]
    fn rt_policy_and_priority_encoded_in_canonical_bit_positions() {
        let features = BinderFeatures {
            min_sched_policy: Some(1), // SCHED_FIFO
            min_priority: Some(42),
            ..Default::default()
        };
        let b = Binder::new_with_features(DummyRemotable, features);
        let flags = b.inner.local_binder_flags();
        assert_eq!(flags & FLAT_BINDER_FLAG_PRIORITY_MASK, 42);
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 1 << 9);
        // INHERIT_RT remains off — caller did not opt in.
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0);
    }

    /// `inherit_rt` sets bit 11 without overlapping the policy and priority fields.
    #[test]
    fn inherit_rt_sets_bit_11_independently_of_policy_field() {
        let features = BinderFeatures {
            inherit_rt: true,
            min_sched_policy: Some(2), // SCHED_RR
            min_priority: Some(99),
            ..Default::default()
        };
        let b = Binder::new_with_features(DummyRemotable, features);
        let flags = b.inner.local_binder_flags();
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0x800);
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 2 << 9);
        assert_eq!(flags & FLAT_BINDER_FLAG_PRIORITY_MASK, 99);
        // Bits are non-overlapping — confirms the layout AC.
        assert_eq!(0x800 & (2u32 << 9), 0);
        assert_eq!(0x800 & 99, 0);
        assert_eq!((2u32 << 9) & 99, 0);
    }

    /// Priority bits above 8 are dropped so they cannot change the SCHED_POLICY field.
    #[test]
    fn out_of_range_priority_is_masked_to_low_byte() {
        let features = BinderFeatures {
            min_priority: Some(0x1234), // bits above 8 must be discarded
            ..Default::default()
        };
        let flags = features.flat_flags();
        assert_eq!(flags & FLAT_BINDER_FLAG_PRIORITY_MASK, 0x34);
        // SCHED_POLICY untouched (no policy set, no bleed from the 0x12).
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0);
    }

    /// Policy bits above 2 are dropped so they cannot set bit 11 (INHERIT_RT) or higher.
    #[test]
    fn out_of_range_policy_is_masked_to_two_bits() {
        let features = BinderFeatures {
            min_sched_policy: Some(0xFF), // would set bits 9..16 unchecked
            ..Default::default()
        };
        let flags = features.flat_flags();
        // Only bits 9-10 (mask 0x600) should be touched; bit 11 must not leak.
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0x600);
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0);
    }
}

/// Stability setters vs AOSP Stability.cpp:41/45/54 and `BBinder::setParceled` (Binder.cpp:725).
#[cfg(test)]
mod stability_mutation_tests {
    use super::*;

    struct DummyRemotable;
    impl crate::Remotable for DummyRemotable {
        fn descriptor() -> &'static str
        where
            Self: Sized,
        {
            "test.stability_mutation"
        }
        fn on_transact(
            &self,
            _: crate::TransactionCode,
            _: &mut crate::Parcel,
            _: &mut crate::Parcel,
        ) -> crate::Result<()> {
            Ok(())
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> crate::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn default_binder_is_system_and_not_parceled() {
        let b = Binder::new(DummyRemotable);
        assert_eq!(b.inner.stability(), Stability::System);
        assert!(!b.inner.was_parceled());
    }

    /// After the upgrade the wire `i32` decodes back to `Vintf`.
    #[test]
    fn mark_vintf_upgrades_to_vintf() {
        let b = Binder::new(DummyRemotable);
        b.inner.mark_vintf().expect("not parceled yet");
        assert_eq!(b.inner.stability(), Stability::Vintf);
        let wire: i32 = b.inner.stability().into();
        // Level byte is low (raw) or high (android-12 Category repr): decode, don't mask.
        assert_eq!(Stability::try_from(wire).unwrap(), Stability::Vintf);
    }

    #[test]
    fn force_downgrade_to_vendor_works_from_system() {
        let b = Binder::new(DummyRemotable);
        b.inner
            .force_downgrade_to_vendor_stability()
            .expect("not parceled yet");
        assert_eq!(b.inner.stability(), Stability::Vendor);
    }

    /// Vintf → System, matching AOSP `forceDowngradeToStability(binder, SYSTEM)`.
    #[test]
    fn vintf_then_force_downgrade_to_system() {
        let b = Binder::new(DummyRemotable);
        b.inner.mark_vintf().unwrap();
        b.inner.force_downgrade_to_system_stability().unwrap();
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// After `set_parceled()` all three setters return `InvalidOperation` (AOSP aborts there).
    #[test]
    fn parceled_guard_blocks_all_setters() {
        let b = Binder::new(DummyRemotable);
        b.inner.set_parceled();
        assert!(b.inner.was_parceled());
        assert_eq!(
            b.inner.mark_vintf().unwrap_err(),
            StatusCode::InvalidOperation
        );
        assert_eq!(
            b.inner.force_downgrade_to_system_stability().unwrap_err(),
            StatusCode::InvalidOperation
        );
        assert_eq!(
            b.inner.force_downgrade_to_vendor_stability().unwrap_err(),
            StatusCode::InvalidOperation
        );
        // The failing setter must not partially update: stability stays at construction level.
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// The same setters routed through `SIBinder`, the handle most callers hold.
    #[test]
    fn sibinder_delegation_path_round_trips() {
        let b = Binder::new(DummyRemotable);
        let si = crate::Interface::as_binder(&b);
        assert_eq!(si.stability(), Stability::System);
        si.mark_vintf().unwrap();
        assert_eq!(si.stability(), Stability::Vintf);
        si.force_downgrade_to_vendor_stability().unwrap();
        assert_eq!(si.stability(), Stability::Vendor);
        assert!(!si.was_parceled());
    }

    /// The cast hands back the same allocation and leaves the strong count where it was.
    #[test]
    fn local_cast_shares_the_allocation() {
        let b = Binder::new(DummyRemotable);
        let si = crate::Interface::as_binder(&b);
        let before = Arc::strong_count(si.as_arc());
        let cast = Binder::<DummyRemotable>::try_from(si.clone()).expect("local binder");
        assert!(Arc::ptr_eq(&cast.inner, &b.inner));
        drop(cast);
        assert_eq!(Arc::strong_count(si.as_arc()), before);
    }

    /// Wraps a local binder and forwards `as_any()`/`descriptor()` to it.
    struct Delegating(SIBinder);

    impl IBinder for Delegating {
        fn link_to_death(&self, r: Weak<dyn DeathRecipient>) -> Result<()> {
            self.0.link_to_death(r)
        }
        fn unlink_to_death(&self, r: Weak<dyn DeathRecipient>) -> Result<()> {
            self.0.unlink_to_death(r)
        }
        fn ping_binder(&self) -> Result<()> {
            self.0.ping_binder()
        }
        fn as_any(&self) -> &dyn Any {
            self.0.as_any()
        }
        fn as_transactable(&self) -> Option<&dyn Transactable> {
            self.0.as_transactable()
        }
        fn descriptor(&self) -> &str {
            self.0.descriptor()
        }
        fn is_remote(&self) -> bool {
            false
        }
        fn inc_strong(&self, _: &SIBinder) -> Result<()> {
            Ok(())
        }
        fn attempt_inc_strong(&self) -> bool {
            true
        }
        fn dec_strong(&self, _: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
            Ok(())
        }
        fn inc_weak(&self, _: &WIBinder) -> Result<()> {
            Ok(())
        }
        fn dec_weak(&self) -> Result<()> {
            Ok(())
        }
    }

    /// The `TypeId` comes from the allocation, so a forwarding `as_any()` cannot pass the cast.
    #[test]
    fn a_wrapper_delegating_as_any_is_not_a_local_binder() {
        let b = Binder::new(DummyRemotable);
        let wrapper = Delegating(crate::Interface::as_binder(&b));
        assert!(wrapper.as_any().is::<Inner<DummyRemotable>>());
        let si = SIBinder::new(Arc::new(wrapper)).expect("SIBinder::new");
        assert_eq!(
            Binder::<DummyRemotable>::try_from(si).err(),
            Some(StatusCode::BadValue)
        );
    }

    /// Constructing with `Vintf` matches `mark_vintf` and stays mutable until parceled.
    #[test]
    fn explicit_vintf_construction_is_observable_and_mutable() {
        let b = Binder::new_with_stability(DummyRemotable, Stability::Vintf);
        assert_eq!(b.inner.stability(), Stability::Vintf);
        b.inner.force_downgrade_to_system_stability().unwrap();
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// AOSP `BBinder::attachObject` / `findObject` / `detachObject` round-trip on one type.
    #[test]
    fn attach_find_detach_round_trip() {
        let b = Binder::new(DummyRemotable);
        #[derive(Debug, PartialEq)]
        struct Side(u32);
        assert!(b.find_object::<Side>().is_none());
        assert!(b.attach_object(Arc::new(Side(42))).is_none());
        let found = b.find_object::<Side>().expect("attached then found");
        assert_eq!(found.0, 42);
        let detached = b.detach_object::<Side>().expect("detached returns");
        assert_eq!(detached.0, 42);
        assert!(b.find_object::<Side>().is_none(), "detach removes entry");
    }

    /// Re-attaching a type returns the stored value, as AOSP does (Binder.cpp:523-531).
    #[test]
    fn attach_object_replaces_returns_old() {
        let b = Binder::new(DummyRemotable);
        struct Side(u32);
        assert!(b.attach_object(Arc::new(Side(1))).is_none());
        let prev = b
            .attach_object(Arc::new(Side(2)))
            .expect("second attach returns old");
        // Downcast for inspection.
        let old: Arc<Side> = prev.downcast::<Side>().expect("type preserved");
        assert_eq!(old.0, 1);
        assert_eq!(b.find_object::<Side>().unwrap().0, 2);
    }

    /// Distinct Rust types coexist in the `TypeId`-keyed map.
    #[test]
    fn distinct_types_do_not_collide() {
        let b = Binder::new(DummyRemotable);
        struct A(u32);
        struct B(&'static str);
        b.attach_object(Arc::new(A(7)));
        b.attach_object(Arc::new(B("hi")));
        assert_eq!(b.find_object::<A>().unwrap().0, 7);
        assert_eq!(b.find_object::<B>().unwrap().0, "hi");
        assert!(b.detach_object::<A>().is_some());
        assert!(b.find_object::<A>().is_none());
        assert!(b.find_object::<B>().is_some(), "B still attached");
    }

    /// After the `Binder` drops, the test's own `Arc` is the only strong reference left.
    #[test]
    fn binder_drop_releases_attached_objects() {
        struct Probe;
        let probe = Arc::new(Probe);
        assert_eq!(Arc::strong_count(&probe), 1);
        {
            let b = Binder::new(DummyRemotable);
            b.attach_object(probe.clone());
            assert_eq!(Arc::strong_count(&probe), 2);
        }
        assert_eq!(
            Arc::strong_count(&probe),
            1,
            "binder drop must release attached arc"
        );
    }

    /// The serializer's `set_parceled()` via `SIBinder` → `dyn IBinder`, without `ProcessState`.
    #[test]
    fn sibinder_set_parceled_via_trait_dispatch_flips_guard() {
        let b = Binder::new(DummyRemotable);
        let si = crate::Interface::as_binder(&b);
        assert!(!si.was_parceled());

        // Exactly what `parcelable::SerializeOption::serialize_option` does after the wire write.
        si.set_parceled();

        assert!(si.was_parceled());
        assert_eq!(
            si.mark_vintf().unwrap_err(),
            StatusCode::InvalidOperation,
            "mutation after parceled-flip must fail"
        );
    }
}

#[cfg(all(test, feature = "rpc"))]
mod rpc_dispatch_tests {
    use super::*;
    use crate::rpc::transport::MemTransport;
    use crate::rpc::{AddressSpace, RpcProxy, RpcSession};

    /// Fails code `FIRST_CALL_TRANSACTION + n` with the `n`th status; `on_dump` writes "dumped".
    struct Svc;
    const FAILS_WITH: [StatusCode; 2] = [StatusCode::DeadObject, StatusCode::PermissionDenied];
    impl Remotable for Svc {
        fn descriptor() -> &'static str {
            "x.y.ISvc"
        }
        fn on_transact(&self, code: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
            let n = code.wrapping_sub(FIRST_CALL_TRANSACTION) as usize;
            Err(FAILS_WITH
                .get(n)
                .copied()
                .unwrap_or(StatusCode::UnknownTransaction))
        }
        fn on_dump(&self, writer: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
            writer.write_all(b"dumped")?;
            Ok(())
        }
    }

    /// The RPC reply status of a handler's `DeadObject` is `FailedTransaction`; others pass.
    #[test]
    fn an_rpc_handler_dead_object_is_not_replied_as_dead_object() {
        let (a, b) = MemTransport::pair();
        let server = RpcSession::new(Box::new(a), AddressSpace::Acceptor).unwrap();
        server
            .set_root(Interface::as_binder(&Binder::new(Svc)))
            .unwrap();
        let handle = std::thread::spawn(move || {
            let _ = server.serve_blocking();
        });
        {
            let client = RpcSession::new(Box::new(b), AddressSpace::Initiator).unwrap();
            let root = client.get_root().unwrap();
            let rp = (*root).as_any().downcast_ref::<RpcProxy>().unwrap();
            let call = |code| {
                let data = rp.build_request(Svc::descriptor()).unwrap();
                rp.transact(code, &data, 0).map(|_| ())
            };
            assert_eq!(
                call(FIRST_CALL_TRANSACTION),
                Err(StatusCode::FailedTransaction)
            );
            assert_eq!(
                call(FIRST_CALL_TRANSACTION + 1),
                Err(StatusCode::PermissionDenied)
            );
        }
        handle.join().unwrap();
    }

    /// An RPC `DUMP_TRANSACTION` takes its fd from the session's table (AOSP `readFileDescriptor`).
    #[test]
    fn an_rpc_dump_writes_to_the_fd_it_carries() {
        use std::io::Read;

        let (read_end, write_end) = rustix::pipe::pipe().unwrap();
        // R34/v0 `Unix` body: fd index 0, then argc 0.
        let mut bytes = 0i32.to_le_bytes().to_vec();
        bytes.extend_from_slice(&0i32.to_le_bytes());
        let mut data = Parcel::data_only_from_vec(bytes);
        data.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
        data.rpc_set_in_fds(vec![write_end]);

        let binder = Interface::as_binder(&Binder::new(Svc));
        let mut reply = Parcel::new();
        binder
            .rpc_transact(DUMP_TRANSACTION, &mut data, &mut reply)
            .unwrap();
        drop(data);
        let mut got = Vec::new();
        File::from(read_end).read_to_end(&mut got).unwrap();
        assert_eq!(got, b"dumped");
    }
}

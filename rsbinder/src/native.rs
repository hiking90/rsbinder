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

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::convert::TryFrom;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::os::fd::FromRawFd;
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
    /// Default: `None` (kernel keeps the caller's policy). Setting this
    /// without `inherit_rt = true` on a real-time policy is undefined —
    /// see [`Self::inherit_rt`] for the canonical RT escalation gate.
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
    /// Default: `false`. The setting only takes effect if `min_sched_policy`
    /// declares an RT policy, otherwise the kernel falls back to the
    /// caller's normal policy.
    pub inherit_rt: bool,
}

impl BinderFeatures {
    /// Encode this feature set into the `flat_binder_object.flags`
    /// bitfield. `FLAT_BINDER_FLAG_ACCEPTS_FDS` is always set —
    /// rsbinder unconditionally accepts file descriptors in its
    /// native binder protocol implementation.
    ///
    /// Bit layout (cross-checked against
    /// `kernel/include/uapi/linux/android/binder.h`):
    ///
    /// ```text
    /// bit 0-7   (0xff):   FLAT_BINDER_FLAG_PRIORITY_MASK
    /// bit 8     (0x100):  FLAT_BINDER_FLAG_ACCEPTS_FDS
    /// bit 9-10  (0x600):  scheduler policy (SHIFT = 9, VALUE_MASK = 0x3)
    /// bit 11    (0x800):  FLAT_BINDER_FLAG_INHERIT_RT
    /// bit 12    (0x1000): FLAT_BINDER_FLAG_TXN_SECURITY_CTX
    /// ```
    pub(crate) fn flat_flags(self) -> u32 {
        let mut f = crate::sys::FLAT_BINDER_FLAG_ACCEPTS_FDS;
        if self.set_requesting_sid {
            f |= crate::sys::FLAT_BINDER_FLAG_TXN_SECURITY_CTX;
        }
        if let Some(priority) = self.min_priority {
            f |= (priority as u32) & crate::sys::FLAT_BINDER_FLAG_PRIORITY_MASK;
        }
        if let Some(policy) = self.min_sched_policy {
            // Only the low 2 bits of policy live in the flat field —
            // higher bits are kernel-internal and would corrupt the
            // adjacent INHERIT_RT bit.
            f |= ((policy as u32) & 0x3) << 9;
        }
        if self.inherit_rt {
            f |= crate::sys::FLAT_BINDER_FLAG_INHERIT_RT;
        }
        f
    }
}

/// Compact tag for [`Stability`] used by [`Inner`]'s atomic field.
///
/// Runtime stability mutation needs interior-mutable storage; an
/// `AtomicU8` with a 4-value tag is cheaper than locking around the
/// `Stability` field and keeps reads on the wire-emit hot path lock-free.
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
        // The only producer is `stability_to_tag`, which emits 0..=3;
        // a different value implies a corrupt `AtomicU8` load. Wire
        // stability is binder-protocol-critical, so abort rather than
        // silently fall through to `System` and emit a mis-stamped
        // `flat_binder_object`.
        other => unreachable!("invalid stability tag {other}: only 0..=3 are stored"),
    }
}

struct Inner<T: Remotable + Send + Sync> {
    remotable: T,
    /// `AtomicU8` storing a [`Stability`] tag (see `STABILITY_TAG_*`).
    /// Atomic so [`Self::force_downgrade_to_system_stability`] and friends
    /// can mutate at runtime without blocking concurrent reads from the
    /// `flat_binder_object` emit path. Default `Relaxed` ordering is
    /// sufficient because the [`Self::parceled`] guard (Acquire/Release)
    /// provides the only correctness-relevant happens-before.
    stability: AtomicU8,
    /// AOSP `BBinder::mParceled` equivalent. Flipped to
    /// `true` the first time this binder is written to a parcel, after
    /// which the runtime stability setters refuse mutation (matches the
    /// `LOG_ALWAYS_FATAL_IF(mParceled, ...)` guards in
    /// [`Binder.cpp:579,606,659,678,713`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=579)).
    parceled: AtomicBool,
    binder_flags: u32,
    strong: RefCounter,
    weak: RefCounter,
    extension: RwLock<Option<SIBinder>>,
    /// Typed object attach/find/detach store. Empty
    /// `HashMap` on construction (no allocation until first
    /// `attach_object`), so the per-binder cost is just the unlocked
    /// `Mutex` header + empty `HashMap` header (~56 bytes on 64-bit).
    ///
    /// AOSP `BBinder::Extras::mObjectMgr` (Binder.cpp:523) keyed by
    /// `const void*` identity; rsbinder keys by `TypeId` to sidestep
    /// the LLVM provenance / ABA hazard of raw pointer identity.
    /// Trade-off: one attached object per Rust type per
    /// binder. Callers that need multiple attachments of the same
    /// concrete type should wrap them in distinct newtype shells.
    objects: Mutex<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl<T: Remotable> Inner<T> {
    /// Gated stability mutation. Refuses if the binder
    /// has already been written to a parcel ([`Self::parceled`] is `true`),
    /// matching the `LOG_ALWAYS_FATAL_IF(mParceled, ...)` guard pattern in
    /// AOSP [`Binder.cpp`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/Binder.cpp;l=579)
    /// (rsbinder returns `Err(InvalidOperation)` rather than aborting).
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
                let obj = _reader.read_object(true)?;
                if obj.header_type() != crate::sys::BINDER_TYPE_FD {
                    return Err(StatusCode::BadType);
                }

                let fd = obj.handle();

                let argc = _reader.read::<i32>()?;
                let mut argv = Vec::new();
                for _ in 0..argc {
                    argv.push(_reader.read::<String>()?);
                }

                // SAFETY:
                // 1. The fd comes from a valid flat_binder_object validated by read_object()
                // 2. We use ManuallyDrop because the kernel owns this fd - it will be closed
                //    by the binder driver when the transaction completes
                // 3. The fd is only valid for the duration of this transaction
                // 4. We only use it for writing (dump output), never transfer ownership
                let mut file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd as _)) };

                self.remotable.on_dump(file.deref_mut(), argv.as_slice())
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

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
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

    /// RPC server dispatch. Mirrors the code
    /// dispatch of `Inner::transact` **minus** the kernel
    /// `check_interface` call and minus the `reader.set_data_position(0)`
    /// reset — the RPC adapter has already consumed + validated the RPC
    /// interface token and positioned `reader` at the arguments. This
    /// neither calls nor changes `Inner::transact` /
    /// `thread_state::check_interface`; it is an additive sibling
    /// entrypoint, so the kernel server path stays bit-identical
    /// (`Inner::transact` untouched). It calls the same generated,
    /// transport-neutral `Remotable::on_transact`.
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
                    // Same fallback as `Inner::transact`: handle
                    // INTERFACE_TRANSACTION etc. via `Inner::on_transact`.
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
                    reply.write(&StatusCode::BadType)?;
                    return Ok(());
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

// This implementation is an idiomatic implementation of the C++
// `IBinder::localBinder` interface if the binder object is a Rust binder
// service.
impl<B: Remotable + 'static> TryFrom<SIBinder> for Binder<B> {
    type Error = StatusCode;

    fn try_from(ibinder: SIBinder) -> Result<Self> {
        if B::descriptor() != ibinder.descriptor() {
            log::error!(
                "Binder type mismatch: expected {}, got {}",
                B::descriptor(),
                ibinder.descriptor()
            );
            return Err(StatusCode::BadType);
        }

        if ibinder.as_any().downcast_ref::<Inner<B>>().is_some() {
            // SAFETY: `downcast_ref::<Inner<B>>` confirmed the
            // underlying allocation is `Inner<B>`. The trait-object
            // Arc's data pointer addresses that `Inner<B>` value
            // directly, so casting `*const dyn IBinder` to
            // `*const Inner<B>` is layout-correct. Cloning the
            // trait-object Arc and consuming it via `Arc::into_raw`
            // leaks one strong ref; `Arc::from_raw` on the cast
            // pointer reclaims that ref as `Arc<Inner<B>>`, conserving
            // the total strong count after `ibinder` drops at the end
            // of this function.
            let arc_dyn = Arc::clone(ibinder.as_arc());
            let raw_dyn = Arc::into_raw(arc_dyn);
            let inner_raw = raw_dyn as *const Inner<B>;
            let inner = unsafe { Arc::from_raw(inner_raw) };

            Ok(Self { inner })
        } else {
            log::error!(
                "Downcast failed: expected {}, got {}",
                B::descriptor(),
                ibinder.descriptor()
            );
            Err(StatusCode::BadValue)
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

    #[test]
    fn requesting_sid_sets_txn_security_ctx() {
        let features = BinderFeatures {
            set_requesting_sid: true,
            ..Default::default()
        };
        let b = Binder::new_with_features(DummyRemotable, features);
        let flags = b.inner.local_binder_flags();
        assert_ne!(flags & FLAT_BINDER_FLAG_TXN_SECURITY_CTX, 0);
        assert_ne!(flags & FLAT_BINDER_FLAG_ACCEPTS_FDS, 0);
    }

    // BinderFeatures sched policy / priority / inherit_rt encoding into
    // flat_binder_object.flags.

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

    /// The default `BinderFeatures` (no RT opt-ins) must produce flat-flags
    /// with RT bit, SCHED_POLICY bits, and priority bits all unset.
    #[test]
    fn default_features_zero_rt_bits() {
        let b = Binder::new(DummyRemotable);
        let flags = b.inner.local_binder_flags();
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0);
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0);
        assert_eq!(flags & FLAT_BINDER_FLAG_PRIORITY_MASK, 0);
    }

    /// Setting an RT policy + priority must produce the exact
    /// AOSP bit pattern — priority in bits 0-7, policy << 9 in bits
    /// 9-10. We pick `SCHED_FIFO = 1` and priority `42` so the result
    /// is unambiguous: `0x100 << 1 | 42 = 0x22A | ACCEPTS_FDS`.
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

    /// `inherit_rt = true` independently flips bit 11 and
    /// composes with the other RT fields without overlap.
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

    /// Priority values larger than 8 bits must be masked
    /// (silently dropped) so they cannot bleed into the adjacent
    /// SCHED_POLICY field — a wire-corruption regression that would
    /// reroute every transaction to a different scheduler class.
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

    /// Scheduler policy values larger than 2 bits must be
    /// masked so they cannot bleed into bit 11 (INHERIT_RT) or higher.
    #[test]
    fn out_of_range_policy_is_masked_to_two_bits() {
        let features = BinderFeatures {
            min_sched_policy: Some(0xFF), // would set bits 9..16 unchecked
            ..Default::default()
        };
        let flags = features.flat_flags();
        // Only bits 9-10 (mask 0x600) should be touched; bit 11 must
        // not leak.
        assert_eq!(flags & FLAT_BINDER_FLAG_SCHED_POLICY_MASK, 0x600);
        assert_eq!(flags & FLAT_BINDER_FLAG_INHERIT_RT, 0);
    }
}

/// Runtime stability mutation API tests.
///
/// AOSP equivalents: `Stability::forceDowngradeToSystemStability` (Stability.cpp:41),
/// `Stability::forceDowngradeToVendorStability` (:45), `Stability::markVintf` (:54),
/// `BBinder::setParceled` (Binder.cpp:725) parceled-guard.
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

    /// Default-constructed binder reports `System`
    /// stability and `was_parceled == false`.
    #[test]
    fn default_binder_is_system_and_not_parceled() {
        let b = Binder::new(DummyRemotable);
        assert_eq!(b.inner.stability(), Stability::System);
        assert!(!b.inner.was_parceled());
    }

    /// `mark_vintf` upgrades `System` to `Vintf` and the
    /// canonical wire bits (`0b111111`) follow.
    #[test]
    fn mark_vintf_upgrades_to_vintf() {
        let b = Binder::new(DummyRemotable);
        b.inner.mark_vintf().expect("not parceled yet");
        assert_eq!(b.inner.stability(), Stability::Vintf);
        let wire: i32 = b.inner.stability().into();
        // Bottom 6 bits are the level; higher bits may carry the
        // Android-12 `0x0c000000` overlay on a real Android target —
        // mask before comparing so the host-tree hermetic test stays
        // build-independent.
        assert_eq!(wire & 0xFF, 0b111111);
    }

    /// `force_downgrade_to_vendor_stability` flips the
    /// declared level to `Vendor`, independent of starting level.
    #[test]
    fn force_downgrade_to_vendor_works_from_system() {
        let b = Binder::new(DummyRemotable);
        b.inner
            .force_downgrade_to_vendor_stability()
            .expect("not parceled yet");
        assert_eq!(b.inner.stability(), Stability::Vendor);
    }

    /// Vintf → System round-trips, matching AOSP
    /// `forceDowngradeToStability(binder, SYSTEM)`.
    #[test]
    fn vintf_then_force_downgrade_to_system() {
        let b = Binder::new(DummyRemotable);
        b.inner.mark_vintf().unwrap();
        b.inner.force_downgrade_to_system_stability().unwrap();
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// Parceled guard: once `set_parceled()` has fired, all
    /// three mutation entry points refuse with `InvalidOperation`.
    /// Mirrors AOSP `LOG_ALWAYS_FATAL_IF(mParceled, ...)` (rsbinder
    /// returns an error rather than aborting, so it composes with the
    /// `Result`-based AIDL surface).
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
        // Stability must remain at construction-time level — the
        // failing setter must not partially update.
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// Same setters routed through `SIBinder` (the public
    /// API surface most callers hold).
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

    /// Explicit `Stability::Vintf` construction should match
    /// what `mark_vintf` would set, and remain mutable until parceled.
    #[test]
    fn explicit_vintf_construction_is_observable_and_mutable() {
        let b = Binder::new_with_stability(DummyRemotable, Stability::Vintf);
        assert_eq!(b.inner.stability(), Stability::Vintf);
        b.inner.force_downgrade_to_system_stability().unwrap();
        assert_eq!(b.inner.stability(), Stability::System);
    }

    /// Round-trip a typed object through
    /// `attach_object`/`find_object`/`detach_object`. Equivalent to
    /// AOSP `BBinder::attachObject` insert + `findObject` lookup +
    /// `detachObject` removal.
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

    /// Re-attaching the same type returns the
    /// previously-stored value, matching AOSP's "old replaced by new"
    /// contract (Binder.cpp:523-531).
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

    /// Distinct Rust types coexist independently
    /// under the TypeId-keyed map.
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

    /// `Binder` drop releases attached object
    /// references. Verified via `Arc::strong_count`: after `Binder`
    /// drops, the external `Arc` should be the only strong ref.
    #[test]
    fn binder_drop_releases_attached_objects() {
        struct Probe(#[allow(dead_code)] u32);
        let probe = Arc::new(Probe(99));
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

    /// Emit-site simulation. The serializer invokes
    /// `binder.set_parceled()` on `&SIBinder` after writing the
    /// `flat_binder_object` + stability int32; this test simulates
    /// that exact dispatch (deref `SIBinder` → `dyn IBinder` →
    /// `Inner<T>::set_parceled`) without pulling in `ProcessState`,
    /// which is not initializable in a host-tree hermetic test.
    #[test]
    fn sibinder_set_parceled_via_trait_dispatch_flips_guard() {
        let b = Binder::new(DummyRemotable);
        let si = crate::Interface::as_binder(&b);
        assert!(!si.was_parceled());

        // Exactly what `parcelable::SerializeOption::serialize_option`
        // does after the wire write (parcelable.rs:484).
        si.set_parceled();

        assert!(si.was_parceled());
        assert_eq!(
            si.mark_vintf().unwrap_err(),
            StatusCode::InvalidOperation,
            "mutation after parceled-flip must fail"
        );
    }
}

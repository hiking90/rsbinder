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

//! Core binder functionality and traits.
//!
//! This module provides the fundamental types and traits for binder IPC,
//! including interface definitions, binder object management, and transaction
//! handling. It forms the foundation for all binder communication.

use std::any::Any;
use std::borrow::Borrow;
use std::fmt::{Debug, Formatter};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::ops::Deref;
use std::sync::{self, Arc};

use crate::{error::*, parcel::*, proxy, sys};

/// Binder action to perform.
///
/// This must be a number between [`FIRST_CALL_TRANSACTION`] and
/// [`LAST_CALL_TRANSACTION`].
pub type TransactionCode = u32;

/// Additional operation flags.
///
/// `FLAG_*` values.
pub type TransactionFlags = u32;

/// Corresponds to TF_ONE_WAY -- an asynchronous call.
pub const FLAG_ONEWAY: TransactionFlags = sys::transaction_flags_TF_ONE_WAY;
/// Corresponds to TF_CLEAR_BUF -- clear transaction buffers after call is made.
pub const FLAG_CLEAR_BUF: TransactionFlags = sys::transaction_flags_TF_CLEAR_BUF;
/// Set to the vendor flag if we are building for the VNDK, 0 otherwise
pub const FLAG_PRIVATE_LOCAL: TransactionFlags = 0;
pub const FLAG_PRIVATE_VENDOR: TransactionFlags = FLAG_PRIVATE_LOCAL;

const fn b_pack_chars(c1: char, c2: char, c3: char, c4: char) -> u32 {
    ((c1 as u32) << 24) | ((c2 as u32) << 16) | ((c3 as u32) << 8) | (c4 as u32)
}

pub const FIRST_CALL_TRANSACTION: u32 = 0x00000001;
pub const LAST_CALL_TRANSACTION: u32 = 0x00ffffff;

pub const PING_TRANSACTION: u32 = b_pack_chars('_', 'P', 'N', 'G');
pub const DUMP_TRANSACTION: u32 = b_pack_chars('_', 'D', 'M', 'P');
pub const SHELL_COMMAND_TRANSACTION: u32 = b_pack_chars('_', 'C', 'M', 'D');
// It must be used to ask binder's interface name.
pub const INTERFACE_TRANSACTION: u32 = b_pack_chars('_', 'N', 'T', 'F');
pub const SYSPROPS_TRANSACTION: u32 = b_pack_chars('_', 'S', 'P', 'R');
pub const EXTENSION_TRANSACTION: u32 = b_pack_chars('_', 'E', 'X', 'T');
pub const DEBUG_PID_TRANSACTION: u32 = b_pack_chars('_', 'P', 'I', 'D');
pub const SET_RPC_CLIENT_TRANSACTION: u32 = b_pack_chars('_', 'R', 'P', 'C');

pub const START_RECORDING_TRANSACTION: u32 = b_pack_chars('_', 'S', 'R', 'D');
pub const STOP_RECORDING_TRANSACTION: u32 = b_pack_chars('_', 'E', 'R', 'D');

// See android.os.IBinder.TWEET_TRANSACTION
// Most importantly, messages can be anything not exceeding 130 UTF-8
// characters, and callees should exclaim "jolly good message old boy!"
pub const TWEET_TRANSACTION: u32 = b_pack_chars('_', 'T', 'W', 'T');

// See android.os.IBinder.LIKE_TRANSACTION
// Improve binder self-esteem.
pub const LIKE_TRANSACTION: u32 = b_pack_chars('_', 'L', 'I', 'K');

pub const INTERFACE_HEADER: u32 = b_pack_chars('S', 'Y', 'S', 'T');

/// Base trait for all binder interfaces.
///
/// This trait allows conversion of a binder interface trait object into an
/// IBinder object for IPC calls. All binder remotable interfaces (i.e., AIDL
/// interfaces) must implement this trait to participate in binder IPC.
///
/// Equivalent to `IInterface` in Android's C++ binder implementation.
pub trait Interface: Send + Sync {
    /// Convert this binder object into a generic [`SIBinder`] reference.
    fn as_binder(&self) -> SIBinder {
        log::error!(
            "as_binder() called on non-Binder object of type {}. \
             This is a programmer error - only Binder objects should implement Interface.",
            std::any::type_name::<Self>()
        );
        // This should never happen in correct code, but we want a clear error
        // rather than undefined behavior
        unreachable!(
            "as_binder() must be overridden by types that can be converted to SIBinder. \
             Type: {}",
            std::any::type_name::<Self>()
        )
    }

    /// Dump transaction handler for this Binder object.
    ///
    /// This handler is a no-op by default and should be implemented for each
    /// Binder service struct that wishes to respond to dump transactions.
    fn dump(&self, _writer: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
        Ok(())
    }
}

/// Trait for converting a generic Binder object into a specific Binder
///
/// # Example
///
/// For Binder interface `IFoo`, the following implementation should be made:
/// ```no_run
/// # use rsbinder::{FromIBinder, SIBinder, Interface, Result, Strong};
/// # trait IFoo : Interface {}
/// impl FromIBinder for dyn IFoo {
///     fn try_from(ibinder: SIBinder) -> Result<Strong<Self>> {
///         // ...
///         # Err(rsbinder::StatusCode::Ok)
///     }
/// }
/// ```
pub trait FromIBinder: Interface {
    /// Try to interpret a generic Binder object as this interface.
    ///
    /// Returns a trait object for the `Self` interface if this object
    /// implements that interface.
    fn try_from(ibinder: SIBinder) -> Result<Strong<Self>>;
}

/// Interface for receiving a notification when a binder object is no longer
/// valid.
///
/// This trait corresponds to the parts of the interface of the C++ `DeathRecipient`
/// Callback interface for binder death notifications.
///
/// Implement this trait to receive notifications when a remote binder object dies.
/// This is essential for cleanup and error handling in distributed systems.
///
/// # Panic safety
///
/// `binder_died` is invoked from the binder worker thread that processes
/// `BR_DEAD_BINDER` from the kernel. rsbinder catches panics from
/// `binder_died` per-recipient and logs them, so:
///
/// - A panic in one recipient does **not** prevent other recipients
///   registered against the same binder from receiving their own
///   `binder_died` callback.
/// - A panic in `binder_died` does **not** terminate the binder worker
///   thread.
///
/// rsbinder does **not** repair the panicking recipient's own internal
/// state across the unwind boundary — implementations should keep
/// `binder_died` panic-free to begin with, and use this guard only as a
/// safety net.
///
/// The guard relies on `panic = "unwind"` (Rust's default). Builds that
/// use `panic = "abort"` abort the whole process on any panic, including
/// from `binder_died`, and the per-recipient isolation degrades to the
/// abort behavior.
pub trait DeathRecipient: Send + Sync {
    /// Called when the monitored binder object has died.
    fn binder_died(&self, who: &WIBinder);
}

/// Core interface for binder objects, both local and remote.
///
/// This trait corresponds to the public interface of the C++ `IBinder` class,
/// providing the fundamental operations available on all binder objects
/// regardless of whether they represent local services or remote proxies.
pub trait IBinder: Any + Send + Sync {
    /// Register the recipient for a notification if this binder
    /// goes away. If this binder object unexpectedly goes away
    /// (typically because its hosting process has been killed),
    /// then the `DeathRecipient`'s callback will be called.
    ///
    /// You will only receive death notifications for remote binders,
    /// as local binders by definition can't die without you dying as well.
    /// Trying to use this function on a local binder will result in an
    /// INVALID_OPERATION code being returned and nothing happening.
    ///
    /// This link always holds a weak reference to its recipient.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()>;

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()>;

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()>;

    /// To support dynamic interface cast, we need to know the interface
    fn as_any(&self) -> &dyn Any;

    /// To convert the interface to a transactable object
    fn as_transactable(&self) -> Option<&dyn Transactable>;

    /// Retrieve the descriptor of this object.
    fn descriptor(&self) -> &str;
    /// Retrieve if this object is remote.
    fn is_remote(&self) -> bool;

    /// Retrieve the stability level of this binder object.
    fn stability(&self) -> Stability {
        Stability::default()
    }

    /// Return the extension binder object, if set. Returns None if no extension is set.
    ///
    /// On a proxy this issues a remote `EXTENSION_TRANSACTION` and
    /// caches the result; subsequent calls return the cached value.
    /// On a native binder it returns the locally-stored extension.
    ///
    /// **Staleness on proxy.** The proxy cache holds a strong
    /// `SIBinder` for the parent's lifetime in the common case (see
    /// `ProxyHandle`'s `ExtensionCache` doc for the cache shape and
    /// the `BC_RELEASE`/`BC_ACQUIRE` thrash motivation). The cache
    /// is **not** invalidated when the extension itself is
    /// obituary'd: subsequent `get_extension()` calls return the same
    /// dead `SIBinder`, and IPC through it fast-fails with
    /// `DeadObject` via `submit_transact`'s obituary check. A server
    /// that re-publishes a fresh extension after the previous one
    /// died is invisible to the client until the parent itself is
    /// dropped and re-acquired. Matches Android C++ `BpBinder` —
    /// not a correctness bug, but worth knowing for clients that
    /// rely on dynamic re-publication.
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        Ok(None)
    }

    /// Set the extension binder object.
    ///
    /// **Server-side only.** A service publishes its extension binder
    /// via this call so clients can discover it through
    /// `get_extension()`. Native binder implementations store the
    /// extension locally; the default returns
    /// `Err(StatusCode::InvalidOperation)`.
    ///
    /// Calling this on a **proxy** (client-side `ProxyHandle`) is a
    /// programming error — a proxy has no way to inform the remote
    /// service of the new extension, so the call would only mutate
    /// the local cache and (after PR #104's §4 scope-down) silently
    /// pin an unrelated `Arc<dyn IBinder>` for the parent's lifetime.
    /// `ProxyHandle::set_extension` therefore rejects with
    /// `InvalidOperation`.
    fn set_extension(&self, _extension: &SIBinder) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }

    fn inc_strong(&self, strong: &SIBinder) -> Result<()>;
    fn attempt_inc_strong(&self) -> bool;
    fn dec_strong(&self, strong: Option<ManuallyDrop<SIBinder>>) -> Result<()>;
    fn inc_weak(&self, weak: &WIBinder) -> Result<()>;
    fn dec_weak(&self) -> Result<()>;
}

impl dyn IBinder {
    /// Convert this binder object into a proxy binder object.
    pub fn as_proxy(&self) -> Option<&proxy::ProxyHandle> {
        self.as_any().downcast_ref::<proxy::ProxyHandle>()
    }
}

impl std::fmt::Debug for dyn IBinder {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:?}", self.as_any())
    }
}

/// Trait for local services that can be exposed via binder IPC.
///
/// Objects implementing this trait can be wrapped in a `Binder<T>` to create
/// a binder service that can receive and handle incoming transactions.
///
/// This is typically auto-generated from AIDL definitions and should not
/// be implemented manually. The AIDL compiler generates the necessary
/// transaction handling code that implements this trait.
pub trait Remotable: Send + Sync {
    /// The Binder interface descriptor string.
    ///
    /// This string is a unique identifier for a Binder interface, and should be
    /// the same between all implementations of that interface.
    fn descriptor() -> &'static str
    where
        Self: Sized;

    /// Handle and reply to a request to invoke a transaction on this object.
    ///
    /// `reply` may be [`None`] if the sender does not expect a reply.
    fn on_transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()>;

    /// Handle a request to invoke the dump transaction on this
    /// object.
    fn on_dump(&self, writer: &mut dyn std::io::Write, args: &[String]) -> Result<()>;
}

/// A transactable object that can be used to process Binder commands.
///
/// # Panic safety
///
/// `transact` is invoked from the binder worker thread that processes
/// incoming `BR_TRANSACTION` from the kernel. rsbinder catches panics
/// from `transact` and converts them to `Err(StatusCode::Unknown)`, so:
///
/// - A panic in `transact` does **not** terminate the binder worker
///   thread; subsequent transactions on the same thread continue to be
///   processed.
/// - For non-oneway calls, the calling client receives a deterministic
///   error reply (`StatusCode::Unknown`) instead of hanging on a never-
///   sent `BR_REPLY`. Any partially-written reply parcel is discarded
///   before the error is returned, so the client does not misparse
///   half-formed data.
/// - For oneway calls, the panic is logged and the transaction is
///   dropped (matching the existing oneway `Err` path).
///
/// rsbinder does **not** repair the panicking transactable's own
/// internal state across the unwind boundary — implementations should
/// keep `transact` panic-free to begin with, and use this guard only
/// as a safety net.
///
/// The guard relies on `panic = "unwind"` (Rust's default). Builds that
/// use `panic = "abort"` abort the whole process on any panic,
/// including from `transact`, and the per-transaction isolation
/// degrades to the abort behavior.
pub trait Transactable: Send + Sync {
    fn transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()>;
}

/// Implemented by sync interfaces to specify what the associated async interface is.
/// Generic to handle the fact that async interfaces are generic over a thread pool.
///
/// The binder in any object implementing this trait should be compatible with the
/// `Target` associated type, and using `FromIBinder` to convert it to the target
/// should not fail.
pub trait ToAsyncInterface<P>
where
    Self: Interface,
    Self::Target: FromIBinder,
{
    /// The async interface associated with this sync interface.
    type Target: ?Sized;
}

/// Implemented by async interfaces to specify what the associated sync interface is.
///
/// The binder in any object implementing this trait should be compatible with the
/// `Target` associated type, and using `FromIBinder` to convert it to the target
/// should not fail.
pub trait ToSyncInterface
where
    Self: Interface,
    Self::Target: FromIBinder,
{
    /// The sync interface associated with this async interface.
    type Target: ?Sized;
}

/// Interface stability promise
///
/// An interface can promise to be a stable vendor interface ([`Stability::Vintf`]), or
/// makes no stability guarantees ([`Stability::Local`]). [`Stability::System`] is
/// the default stability, matching Android's `getLocalLevel()` for non-VNDK builds.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub enum Stability {
    /// Default stability, visible to other modules in the same compilation
    /// context (e.g. modules on system.img)
    Local,
    Vendor,
    #[default]
    System,

    /// A Vendor Interface Object, which promises to be stable
    Vintf,
}

impl Stability {
    /// Android bitmask-compatible verification.
    ///
    /// Checks whether `self` (provider) includes the `required` stability level.
    /// e.g., Vintf includes all levels. System and Vendor are independent domains.
    pub fn includes(&self, required: Stability) -> bool {
        let provided: i32 = (*self).into();
        let required: i32 = required.into();
        (provided & required) == required
    }
}

// Android 12 version uses "Category" as the stability format for passed on the wire lines,
// whereas other versions do not. Therefore, we can use the android_properties crate
// to determine the Android version and perform different handling accordingly.
// http://aospxref.com/android-11.0.0_r21/xref/frameworks/native/libs/binder/include/binder/Stability.h
// http://aospxref.com/android-12.0.0_r3/xref/frameworks/native/include/binder/Stability.h
// http://aospxref.com/android-13.0.0_r3/xref/frameworks/native/libs/binder/include/binder/Stability.h
// http://aospxref.com/android-14.0.0_r2/xref/frameworks/native/libs/binder/include/binder/Stability.h
impl From<Stability> for i32 {
    fn from(stability: Stability) -> i32 {
        use Stability::*;

        let stability = match stability {
            Local => 0,
            Vendor => 0b000011,
            System => 0b001100,
            Vintf => 0b111111,
        };

        #[cfg(target_os = "android")]
        {
            let android_sdk_version = crate::get_android_sdk_version();
            if android_sdk_version == 31 || android_sdk_version == 32 {
                stability | 0x0c000000
            } else {
                stability
            }
        }
        #[cfg(not(target_os = "android"))]
        stability
    }
}

impl TryFrom<i32> for Stability {
    type Error = StatusCode;
    fn try_from(stability: i32) -> Result<Stability> {
        use Stability::*;

        // Try matching as raw Level value first (Android 11, 13+)
        let level = if stability <= 0xFF {
            stability
        } else {
            // Try extracting level from Category repr format (Android 12).
            // Category struct on little-endian: { version: u8, reserved: [u8; 2], level: u8 }
            // repr() = (level << 24) | version
            (stability >> 24) & 0xFF
        };

        match level {
            0 => Ok(Local),
            0b000011 => Ok(Vendor),
            0b001100 => Ok(System),
            0b111111 => Ok(Vintf),
            _ => {
                log::error!("Stability value is invalid: {stability:#X}");
                Err(StatusCode::BadValue)
            }
        }
    }
}

/// Strong reference to a binder object.
pub struct SIBinder {
    inner: Arc<dyn IBinder>,
}

impl SIBinder {
    /// Wrap an `Arc<dyn IBinder>` in an `SIBinder`.
    ///
    /// Drives `inc_strong` on the inner binder. For native binders this
    /// advances the local `RefCounter.strong`; for proxies it is a no-op
    /// (proxy ref-count is owned by the cache-pin model — see
    /// `proxy::ProxyHandle`).
    pub fn new(data: Arc<dyn IBinder>) -> Result<Self> {
        let this = Self { inner: data };
        this.increase()?;
        Ok(this)
    }

    pub(crate) fn from_arc(inner: Arc<dyn IBinder>) -> Self {
        // Used on the construction path inside `process_state` after
        // `ProxyHandle::new_acquired` has already issued `BC_ACQUIRE`.
        // For proxies, `inc_strong` is a no-op so this is equivalent to
        // `Self::new`. Kept as a separate constructor so the proxy
        // resurrection path makes its no-op-on-proxy intent explicit.
        let this = Self { inner };
        // Native binders still need their `RefCounter.strong` driven
        // here; proxies will short-circuit to `Ok(())`.
        this.increase()
            .expect("inc_strong on existing Arc<dyn IBinder> must not fail");
        this
    }

    /// Borrow the underlying `Arc<dyn IBinder>`.
    ///
    /// Provides a non-consuming view of the inner trait-object Arc so
    /// callers can clone it for sidecar tables (e.g.
    /// `ProcessState::published_natives`) or compare identity via
    /// `Arc::as_ptr` without an unsafe round trip through raw pointers.
    pub(crate) fn as_arc(&self) -> &Arc<dyn IBinder> {
        &self.inner
    }

    /// Construct a weak reference to this binder.
    ///
    /// Pure `Arc::downgrade` — no kernel command, no trait dispatch.
    ///
    /// For proxies, the resulting `WIBinder` snapshots `(handle,
    /// stability, generation)` so a later `WIBinder::upgrade()` can
    /// route through the process-wide proxy cache. As long as the
    /// cache pin is alive (no obituary yet), `upgrade()` succeeds via
    /// case-(b) resurrection — matching Android `wp<BpBinder>::promote()`
    /// semantics. If the cache entry was removed (obituary) or the
    /// handle id was recycled to a different binder_node (generation
    /// mismatch), `upgrade()` returns `Err(DeadObject)`.
    ///
    /// For native binders, the `WIBinder` is a plain
    /// `sync::Weak<dyn IBinder>` — `upgrade()` succeeds iff some
    /// `Arc<dyn IBinder>` to the inner binder is still alive.
    pub fn downgrade(this: &Self) -> WIBinder {
        let weak = Arc::downgrade(&this.inner);
        if let Some(proxy_handle) = this.inner.as_any().downcast_ref::<proxy::ProxyHandle>() {
            // For proxies, capture handle/stability/generation. The cache
            // entry MUST exist at this point — `Arc<ProxyHandle>` is alive,
            // and `strong_proxy_for_handle_stability` always inserts the
            // entry alongside allocating the Arc under the same write lock.
            // If we somehow miss it (race window during obituary), fall
            // back to Native — `upgrade` will then see a dangling weak and
            // return DeadObject, which is the correct contract.
            let handle = proxy_handle.handle();
            let stability = proxy_handle.stability();
            let generation =
                crate::process_state::ProcessState::as_self().cache_generation_for(handle);
            match generation {
                Some(generation) => WIBinder {
                    inner: WIBinderInner::Proxy {
                        handle,
                        stability,
                        generation,
                        weak,
                    },
                },
                None => WIBinder {
                    inner: WIBinderInner::Native(weak),
                },
            }
        } else {
            WIBinder {
                inner: WIBinderInner::Native(weak),
            }
        }
    }

    /// Retrieve the stability level of the underlying binder object.
    pub fn stability(&self) -> Stability {
        self.inner.stability()
    }

    pub(crate) fn increase(&self) -> Result<()> {
        self.inner.inc_strong(self)
    }

    pub(crate) fn attempt_increase(&self) -> bool {
        self.inner.attempt_inc_strong()
    }

    pub(crate) fn decrease(&self) -> Result<()> {
        self.inner.dec_strong(None)
    }

    /// Try to convert this Binder object into a trait object for the given
    /// Binder interface.
    ///
    /// If this object does not implement the expected interface, the error
    /// `StatusCode::BadType` is returned.
    pub fn into_interface<I: FromIBinder + Interface + ?Sized>(self) -> Result<Strong<I>> {
        FromIBinder::try_from(self)
    }
}

impl Debug for SIBinder {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        f.debug_struct("SIBinder")
            .field("descriptor", &self.descriptor())
            .field("ptr", &Arc::as_ptr(&self.inner))
            .finish()
    }
}

impl Clone for SIBinder {
    fn clone(&self) -> Self {
        Self::from_arc(Arc::clone(&self.inner))
    }
}

impl Drop for SIBinder {
    fn drop(&mut self) {
        self.decrease()
            .map_err(|err| {
                log::error!("Error in SIBinder::drop() is {err:?}");
            })
            .ok();
    }
}

impl Deref for SIBinder {
    type Target = dyn IBinder;
    fn deref(&self) -> &Self::Target {
        self.inner.deref()
    }
}

impl PartialEq for SIBinder {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

impl Eq for SIBinder {}

/// Weak reference to a binder object.
///
/// `upgrade()` is fallible. The conditions under which it succeeds
/// depend on whether the underlying binder is a proxy (remote) or a
/// native (local) one:
///
/// **Proxy.** `upgrade()` succeeds as long as the process-wide proxy
/// cache entry for this binder is alive (no obituary yet) and the
/// handle id has not been recycled to a different binder_node since
/// this `WIBinder` was created. Specifically:
///   - If some `Arc<ProxyHandle>` is still alive in the process,
///     `upgrade()` returns it directly (analogous to a cache hit).
///   - Otherwise the cache pin (`BC_INCREFS` taken at first
///     resolution) keeps the kernel `binder_ref` slot alive, so a
///     fresh `BC_ACQUIRE` succeeds and `upgrade()` returns a freshly
///     allocated `Arc<ProxyHandle>` (case-(b) resurrection). The
///     resurrected `Arc` is a different allocation than the original,
///     but refers to the same kernel binder_node.
///   - If `BR_DEAD_BINDER` was processed for this handle, the cache
///     entry is gone and `upgrade()` returns `Err(DeadObject)`.
///   - If the handle id was recycled to a different binder_node since
///     this `WIBinder` was created, the snapshotted generation does
///     not match the live entry's, and `upgrade()` returns
///     `Err(DeadObject)`.
///
/// This matches Android `wp<BpBinder>::promote()` semantics: weak
/// references can be promoted to strong as long as the binder is
/// still alive in the kernel, even if every user-side strong ref has
/// been dropped.
///
/// **Native.** `upgrade()` succeeds iff some `Arc<dyn IBinder>` to the
/// inner binder is still alive in the process. This is plain
/// `sync::Weak::upgrade` semantics — natives have no process-wide
/// cache, so when the last strong is dropped, the binder is gone.
pub struct WIBinder {
    inner: WIBinderInner,
}

pub(crate) enum WIBinderInner {
    /// Proxy weak reference. Carries `(handle, stability, generation)`
    /// snapshot so `upgrade` can route through the proxy cache for
    /// resurrection.
    Proxy {
        handle: u32,
        stability: Stability,
        generation: u64,
        weak: sync::Weak<dyn IBinder>,
    },
    /// Native weak reference. Plain `sync::Weak`.
    Native(sync::Weak<dyn IBinder>),
}

impl WIBinder {
    pub fn upgrade(&self) -> Result<SIBinder> {
        match &self.inner {
            WIBinderInner::Native(weak) => weak
                .upgrade()
                .map(SIBinder::from_arc)
                .ok_or(StatusCode::DeadObject),
            WIBinderInner::Proxy {
                handle,
                stability,
                generation,
                weak,
            } => {
                // Fast path: an `Arc<ProxyHandle>` is still alive
                // somewhere in the process — reuse it without touching
                // the cache lock.
                if let Some(arc) = weak.upgrade() {
                    return Ok(SIBinder::from_arc(arc));
                }
                // Slow path: drive cache-(b) resurrection. The cache
                // pin keeps the kernel binder_ref slot alive, so the
                // BC_ACQUIRE inside `new_acquired` is guaranteed to
                // succeed unless the entry was obituary'd or recycled
                // (in which case we get DeadObject).
                crate::process_state::ProcessState::as_self().resurrect_proxy_for_handle_stability(
                    *handle,
                    *stability,
                    *generation,
                )
            }
        }
    }
}

impl Debug for WIBinder {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match &self.inner {
            WIBinderInner::Native(weak) => f
                .debug_struct("WIBinder::Native")
                .field("strong_count", &weak.strong_count())
                .field("weak_count", &weak.weak_count())
                .finish(),
            WIBinderInner::Proxy {
                handle,
                stability,
                generation,
                weak,
            } => f
                .debug_struct("WIBinder::Proxy")
                .field("handle", handle)
                .field("stability", stability)
                .field("generation", generation)
                .field("strong_count", &weak.strong_count())
                .finish(),
        }
    }
}

impl Clone for WIBinder {
    fn clone(&self) -> Self {
        let inner = match &self.inner {
            WIBinderInner::Native(weak) => WIBinderInner::Native(sync::Weak::clone(weak)),
            WIBinderInner::Proxy {
                handle,
                stability,
                generation,
                weak,
            } => WIBinderInner::Proxy {
                handle: *handle,
                stability: *stability,
                generation: *generation,
                weak: sync::Weak::clone(weak),
            },
        };
        Self { inner }
    }
}

impl PartialEq for WIBinder {
    /// Equality matches Android `BpBinder` identity. For proxies, two
    /// `WIBinder`s are equal iff they refer to the same kernel
    /// binder_node — i.e. same `(handle, generation)` pair. This holds
    /// even across resurrection (the new `Arc<ProxyHandle>` is a fresh
    /// allocation, so `sync::Weak::ptr_eq` would say "different", but
    /// they are conceptually the same binder). For natives, equality
    /// is `sync::Weak::ptr_eq` on the inner Arc allocation.
    fn eq(&self, other: &Self) -> bool {
        match (&self.inner, &other.inner) {
            (WIBinderInner::Native(a), WIBinderInner::Native(b)) => sync::Weak::ptr_eq(a, b),
            (
                WIBinderInner::Proxy {
                    handle: ha,
                    generation: ga,
                    ..
                },
                WIBinderInner::Proxy {
                    handle: hb,
                    generation: gb,
                    ..
                },
            ) => ha == hb && ga == gb,
            _ => false,
        }
    }
}

/// Strong reference to a binder object
pub struct Strong<I: FromIBinder + ?Sized>(Box<I>);

impl<I: FromIBinder + ?Sized> Strong<I> {
    /// Create a new strong reference to the provided binder object
    pub fn new(binder: Box<I>) -> Self {
        Self(binder)
    }

    /// Construct a new weak reference to this binder
    pub fn downgrade(this: &Strong<I>) -> Weak<I> {
        Weak::new(this)
    }

    /// Convert this synchronous binder handle into an asynchronous one.
    pub fn into_async<P>(self) -> Strong<<I as ToAsyncInterface<P>>::Target>
    where
        I: ToAsyncInterface<P>,
    {
        // By implementing the ToAsyncInterface trait, it is guaranteed that the binder
        // object is also valid for the target type.
        FromIBinder::try_from(self.0.as_binder())
            .expect("ToAsyncInterface guarantees binder compatibility")
    }

    /// Convert this asynchronous binder handle into a synchronous one.
    pub fn into_sync(self) -> Strong<<I as ToSyncInterface>::Target>
    where
        I: ToSyncInterface,
    {
        // By implementing the ToSyncInterface trait, it is guaranteed that the binder
        // object is also valid for the target type.
        FromIBinder::try_from(self.0.as_binder())
            .expect("ToSyncInterface guarantees binder compatibility")
    }
}

impl<I: FromIBinder + ?Sized> Clone for Strong<I> {
    fn clone(&self) -> Self {
        // Since we hold a strong reference, we should always be able to create
        // a new strong reference to the same interface type, so try_from()
        // should never fail here.
        FromIBinder::try_from(self.0.as_binder())
            .expect("Failed to clone Strong<I>: existing strong reference guarantees valid binder")
    }
}

impl<I: FromIBinder + ?Sized> Borrow<I> for Strong<I> {
    fn borrow(&self) -> &I {
        &self.0
    }
}

impl<I: FromIBinder + ?Sized> AsRef<I> for Strong<I> {
    fn as_ref(&self) -> &I {
        &self.0
    }
}

impl<I: FromIBinder + ?Sized> Deref for Strong<I> {
    type Target = I;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<I: FromIBinder + Debug + ?Sized> Debug for Strong<I> {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        Debug::fmt(&**self, f)
    }
}

impl<I: FromIBinder + ?Sized> PartialEq for Strong<I> {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_binder().eq(&other.0.as_binder())
    }
}

impl<I: FromIBinder + ?Sized> Eq for Strong<I> {}

/// Weak reference to a binder object
#[derive(Debug)]
pub struct Weak<I: FromIBinder + ?Sized> {
    weak_binder: WIBinder,
    interface_type: PhantomData<I>,
}

impl<I: FromIBinder + ?Sized> Weak<I> {
    /// Construct a new weak reference from a strong reference
    fn new(binder: &Strong<I>) -> Self {
        let weak_binder = SIBinder::downgrade(&binder.as_binder());
        Weak {
            weak_binder,
            interface_type: PhantomData,
        }
    }

    /// Upgrade this weak reference to a strong reference if the binder object
    /// is still alive
    pub fn upgrade(&self) -> Result<Strong<I>> {
        self.weak_binder.upgrade().and_then(FromIBinder::try_from)
    }
}

impl<I: FromIBinder + ?Sized> Clone for Weak<I> {
    fn clone(&self) -> Self {
        Self {
            weak_binder: self.weak_binder.clone(),
            interface_type: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    // use crate::proxy::ProxyHandle;
    use super::*;

    /// Minimal `IBinder` impl for unit-testing the `SIBinder` / `WIBinder`
    /// pair without needing `/dev/binderfs/binder`. Ref-count methods are
    /// no-ops — the fallibility test below relies on Rust `Arc`/`Weak`
    /// semantics, not on `RefCounter` state.
    struct MockBinder;

    impl IBinder for MockBinder {
        fn link_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
            Err(StatusCode::InvalidOperation)
        }
        fn unlink_to_death(&self, _: sync::Weak<dyn DeathRecipient>) -> Result<()> {
            Err(StatusCode::InvalidOperation)
        }
        fn ping_binder(&self) -> Result<()> {
            Ok(())
        }
        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn as_transactable(&self) -> Option<&dyn Transactable> {
            None
        }
        fn descriptor(&self) -> &str {
            "rsbinder.test.MockBinder"
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

    /// Verifies the `WIBinder::upgrade()` semantic correction in this PR:
    /// the inner reference is now a genuine `sync::Weak<dyn IBinder>`, so
    /// once the last `Arc<dyn IBinder>` is dropped, `upgrade()` returns
    /// `Err(StatusCode::DeadObject)` instead of (incorrectly) succeeding
    /// against a strong-Arc-in-disguise.
    #[test]
    fn test_wibinder_upgrade_after_strong_drop_returns_dead_object() {
        let strong = SIBinder::new(Arc::new(MockBinder)).expect("SIBinder::new");
        let weak = SIBinder::downgrade(&strong);

        // While `strong` is alive, upgrade succeeds and yields a binder
        // pointing at the same allocation.
        let upgraded = weak.upgrade().expect("upgrade while alive");
        assert!(Arc::ptr_eq(&strong.inner, &upgraded.inner));
        drop(upgraded);

        // Drop the only strong holder; the underlying Arc's strong count
        // drops to 0. Now upgrade must fail with DeadObject.
        drop(strong);
        match weak.upgrade() {
            Err(StatusCode::DeadObject) => {}
            Err(other) => panic!("expected DeadObject after Arc drop, got {other:?}"),
            Ok(_) => panic!(
                "expected DeadObject after Arc drop, but upgrade succeeded \
                 (regression: WIBinder::upgrade is no longer truly weak)"
            ),
        }
    }

    /// `Weak<I>::upgrade()` is the typed equivalent and shares the same
    /// semantic. Verify the typed wrapper also surfaces DeadObject. Uses
    /// the same MockBinder; we don't need a real Remotable + AIDL stack
    /// to exercise the fallibility — we go through `WIBinder::upgrade`
    /// and stop at `FromIBinder::try_from`'s expected failure mode.
    #[test]
    fn test_wibinder_clone_and_ptr_eq_after_drop() {
        let strong = SIBinder::new(Arc::new(MockBinder)).expect("SIBinder::new");
        let weak1 = SIBinder::downgrade(&strong);
        let weak2 = weak1.clone();

        // Two clones of the same WIBinder must compare equal via ptr_eq.
        assert_eq!(weak1, weak2);

        drop(strong);
        // After drop, both still ptr_eq each other (Weak::ptr_eq compares
        // allocation addresses, which remain stable).
        assert_eq!(weak1, weak2);
        // ...but neither upgrades.
        assert!(matches!(weak1.upgrade(), Err(StatusCode::DeadObject)));
        assert!(matches!(weak2.upgrade(), Err(StatusCode::DeadObject)));
    }

    #[test]
    fn test_strong() -> Result<()> {
        // let descriptor = "interface";
        // let strong = SIBinder::new(Box::new(ProxyHandle::new(0, descriptor, Default::default())), descriptor);
        // assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 1);

        // let strong2 = strong.clone();
        // assert_eq!(strong2.inner.strong.load(Ordering::Relaxed), 2);

        // let weak = SIBinder::downgrade(&strong);

        // assert_eq!(weak.inner.strong.load(Ordering::Relaxed), 1);

        // let strong = weak.upgrade();
        // assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 2);
        // SIBinder::downgrade(&strong);
        // assert_eq!(*strong2.0.lock().unwrap(), 101);

        // let weak = strong2.downgrade();

        // assert_eq!(*weak.0.lock().unwrap(), 1);

        Ok(())
    }

    #[test]
    fn test_b_pack_chars() {
        assert_eq!(b_pack_chars('_', 'P', 'N', 'G'), PING_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'D', 'M', 'P'), DUMP_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'C', 'M', 'D'), SHELL_COMMAND_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'N', 'T', 'F'), INTERFACE_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'S', 'P', 'R'), SYSPROPS_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'E', 'X', 'T'), EXTENSION_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'P', 'I', 'D'), DEBUG_PID_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'R', 'P', 'C'), SET_RPC_CLIENT_TRANSACTION);
        assert_eq!(
            b_pack_chars('_', 'S', 'R', 'D'),
            START_RECORDING_TRANSACTION
        );
        assert_eq!(b_pack_chars('_', 'E', 'R', 'D'), STOP_RECORDING_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'T', 'W', 'T'), TWEET_TRANSACTION);
        assert_eq!(b_pack_chars('_', 'L', 'I', 'K'), LIKE_TRANSACTION);
    }
}

#[cfg(test)]
mod stability_tests {
    use super::*;

    // === Wire value tests ===
    #[test]
    fn test_stability_wire_values() {
        assert_eq!(Into::<i32>::into(Stability::Local), 0);
        assert_eq!(Into::<i32>::into(Stability::Vendor), 0b000011);
        assert_eq!(Into::<i32>::into(Stability::System), 0b001100);
        assert_eq!(Into::<i32>::into(Stability::Vintf), 0b111111);
    }

    // === TryFrom round-trip tests ===
    #[test]
    fn test_stability_roundtrip() {
        for s in [
            Stability::Local,
            Stability::Vendor,
            Stability::System,
            Stability::Vintf,
        ] {
            let wire: i32 = s.into();
            assert_eq!(Stability::try_from(wire).unwrap(), s);
        }
    }

    #[test]
    fn test_stability_category_format() {
        // Android 12 Category repr: level in byte 3 (big end on little-endian)
        // Category{version=1, reserved=[0,0], level} → repr = (level << 24) | version
        let category = |level: i32, version: i32| -> i32 { (level << 24) | version };

        assert_eq!(
            Stability::try_from(category(0b000011, 1)).unwrap(),
            Stability::Vendor
        );
        assert_eq!(
            Stability::try_from(category(0b001100, 1)).unwrap(),
            Stability::System
        );
        assert_eq!(
            Stability::try_from(category(0b111111, 1)).unwrap(),
            Stability::Vintf
        );

        // Different version values should also work
        assert_eq!(
            Stability::try_from(category(0b001100, 12)).unwrap(),
            Stability::System
        );

        // rsbinder's own Android 12 format: level | 0x0c000000
        assert_eq!(
            Stability::try_from(0b001100 | 0x0c000000_i32).unwrap(),
            Stability::System
        );
    }

    #[test]
    fn test_stability_invalid_value() {
        assert_eq!(Stability::try_from(0x7F).unwrap_err(), StatusCode::BadValue);
        assert_eq!(Stability::try_from(0x01).unwrap_err(), StatusCode::BadValue);
    }

    // === Bitmask verification tests ===
    #[test]
    fn test_stability_includes() {
        // Local is included by all stability levels
        assert!(Stability::Local.includes(Stability::Local));
        assert!(Stability::Vendor.includes(Stability::Local));
        assert!(Stability::System.includes(Stability::Local));
        assert!(Stability::Vintf.includes(Stability::Local));

        // Vendor and System are independent domains
        assert!(!Stability::Vendor.includes(Stability::System));
        assert!(!Stability::System.includes(Stability::Vendor));

        // Vintf includes all levels
        assert!(Stability::Vintf.includes(Stability::Vendor));
        assert!(Stability::Vintf.includes(Stability::System));
        assert!(Stability::Vintf.includes(Stability::Vintf));

        // Self-inclusion
        assert!(Stability::Vendor.includes(Stability::Vendor));
        assert!(Stability::System.includes(Stability::System));
    }
}

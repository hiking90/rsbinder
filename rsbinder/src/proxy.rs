// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client proxy for remote binder services.
//!
//! This module provides the client-side infrastructure for communicating with
//! remote binder services, including proxy objects that represent remote services
//! and handle transaction routing and lifecycle management.

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::mem::ManuallyDrop;
use std::os::fd::IntoRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{self, Arc, RwLock};

use crate::{
    binder::*, binder_object::*, error::*, parcel::*, parcelable::DeserializeOption, thread_state,
};

/// Cache state for the extension binder object on the proxy side.
///
/// The payload is split into two variants because the right ref-count
/// discipline depends on whether the extension's handle aliases the
/// parent proxy's own handle:
///
///   * **Common case (`CachedExtension::Strong`)** — the extension is a
///     different binder. We hold an `SIBinder` so the extension's
///     `Arc<ProxyHandle>` is rooted by the parent proxy's cache for as
///     long as the parent itself lives. A weak cache here would let the
///     extension's `Arc<ProxyHandle>` drop and be resurrected on every
///     `get_extension` cycle, producing a stream of `BC_RELEASE`/
///     `BC_ACQUIRE` pairs against the kernel binder_ref. Under stress,
///     the `binder-linux` driver has been observed to lose the
///     `binder_ref → binder_node` association across that thrash and
///     return `BR_FAILED_REPLY` ("cannot find target node") on the very
///     next transaction. Stable strong caching avoids the thrash
///     entirely and matches the PR #102 baseline.
///
///   * **Self-cycle case (`CachedExtension::Weak`)** — the extension's
///     handle equals the parent's own handle (a remote naming itself as
///     its own extension). A strong cache here would form a
///     self-referencing `Arc<ProxyHandle>` cycle through the parent's
///     own state and prevent the parent from ever being dropped. We
///     fall back to `WIBinder` for this case only; the user must hold
///     an external strong ref to the parent for the extension to be
///     reachable, and `weak.upgrade()` always succeeds via the
///     fast-path Arc reuse without invoking cache-pin resurrection — so
///     there is no `BC_RELEASE`/`BC_ACQUIRE` thrash in this case
///     either.
enum ExtensionCache {
    /// Remote query has not been performed yet.
    NotQueried,
    /// Remote query completed; stores the result (Some or None).
    Queried(Option<CachedExtension>),
}

enum CachedExtension {
    /// Common case: extension proxy distinct from the parent proxy.
    Strong(SIBinder),
    /// Degenerate case: extension's handle aliases the parent's own
    /// handle. Stored as weak to avoid an `Arc<ProxyHandle>` self-cycle.
    Weak(WIBinder),
}

/// Handle for a proxy to a remote binder service.
///
/// Owns exactly **one kernel strong ref** (`BC_ACQUIRE` at construction,
/// `BC_RELEASE` on `Drop`). The kernel weak ref that keeps the
/// `binder_ref` slot alive across `strong = 0` windows is held by the
/// process-wide cache pin (`ProcessState::handle_to_proxy`), not by this
/// type — see `process_state::strong_proxy_for_handle_stability`.
pub struct ProxyHandle {
    handle: u32,
    descriptor: String,
    stability: Stability,
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<sync::Weak<dyn DeathRecipient>>>,
    extension: RwLock<ExtensionCache>,
}

impl ProxyHandle {
    /// Allocate a fresh `Arc<ProxyHandle>` and acquire one kernel strong ref
    /// (`BC_ACQUIRE`). Caller must hold the `ProcessState::handle_to_proxy`
    /// write lock and must have already issued+flushed the cache pin
    /// (`BC_INCREFS`) for `handle` (sub-case (a)) or verified that an
    /// existing cache entry's pin is still active (sub-case (b)) — the pin
    /// keeps the `binder_ref` slot alive, so this `BC_ACQUIRE` cannot race
    /// against a concurrent `BC_RELEASE` to a freed slot.
    pub(crate) fn new_acquired(
        handle: u32,
        descriptor: String,
        stability: Stability,
    ) -> Result<Arc<Self>> {
        let arc = Arc::new(Self {
            handle,
            descriptor,
            stability,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
            extension: RwLock::new(ExtensionCache::NotQueried),
        });
        thread_state::inc_strong_handle(handle)?;
        Ok(arc)
    }

    /// Get the underlying binder handle number.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Get the interface descriptor for this proxy.
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// Pick the right cache representation for an extension binder.
    ///
    /// Returns `CachedExtension::Weak` only when the extension is a
    /// proxy whose handle aliases this proxy's own handle — the
    /// self-cycle case where a strong cache would form an
    /// `Arc<ProxyHandle>` cycle through the parent's own state.
    /// Everything else (extension is a different proxy, extension is a
    /// local binder, extension is a proxy with a different handle)
    /// uses `Strong` so the extension's `Arc<ProxyHandle>` is rooted by
    /// the parent's cache and we avoid the `BC_RELEASE`/`BC_ACQUIRE`
    /// thrash documented on `ExtensionCache`.
    fn classify_extension(&self, sib: &SIBinder) -> CachedExtension {
        if let Some(proxy) = (**sib).as_proxy() {
            if proxy.handle() == self.handle {
                return CachedExtension::Weak(SIBinder::downgrade(sib));
            }
        }
        CachedExtension::Strong(sib.clone())
    }

    /// Submit a transaction to the remote service.
    pub fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        // Fast-fail after obituary: avoid a futile kernel round trip
        // that would only return BR_DEAD_REPLY. Mirrors C++
        // `BpBinder::transact` (BpBinder.cpp:337) which checks `mAlive`
        // outside `mLock` for the same reason. The Acquire load pairs
        // with the Release store inside `send_obituary`'s recipients
        // lock — observing `true` here implies a happens-before with
        // the obituary teardown.
        if self.obituary_sent.load(Ordering::Acquire) {
            return Err(StatusCode::DeadObject);
        }
        thread_state::transact(self.handle(), code, data, flags)
    }

    pub fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        let mut data = Parcel::new();

        if write_header {
            data.write_interface_token(self.descriptor())?;
        }

        Ok(data)
    }

    pub(crate) fn send_obituary(&self, who: &WIBinder) -> Result<()> {
        // Mirrors C++ `BpBinder::sendObituary` (BpBinder.cpp:489–528):
        //   1. All `mObitsSent` reads/writes happen under `mLock`.
        //   2. The `mObituaries` vector is detached under `mLock` and
        //      `mLock.unlock()` is called BEFORE invoking
        //      `reportOneDeath` callbacks, so a callback may safely
        //      re-enter `linkToDeath`/`unlinkToDeath`.
        //
        // Without (1), a `link_to_death` racing with `send_obituary`
        // could push a recipient into the just-drained vector and the
        // recipient would never fire. Without (2), a callback that
        // calls `unlink_to_death` on `self` would deadlock against the
        // held recipients lock.
        //
        // Idempotency: a second `send_obituary` (e.g. spurious double
        // BR_DEAD_BINDER) takes the lock, sees `obituary_sent == true`,
        // and returns immediately — matching C++ line 500's
        // `if (mObitsSent) return;`.
        //
        // Error handling: queue BC_CLEAR_DEATH_NOTIFICATION BEFORE
        // `mem::take` so a queueing failure leaves recipients intact
        // for retry. Callbacks fire BEFORE the IPC flush so a
        // `flush_commands` error does not swallow the obituary —
        // matches C++ which ignores `clearDeathNotification` /
        // `flushCommands` return values entirely.
        let recipients_snapshot: Vec<sync::Weak<dyn DeathRecipient>> = {
            let mut recipients = self.recipients.write().expect("Recipients lock poisoned");

            // Lock-protected check + set, like C++ lines 500/515.
            // `Relaxed` here is sufficient because the surrounding
            // RwLock acquire/release supplies all the happens-before we
            // need against other lock-protected sites.
            if self.obituary_sent.load(Ordering::Relaxed) {
                return Ok(());
            }

            if !recipients.is_empty() {
                // Queue BC_CLEAR before draining. On queueing failure
                // the recipients vector is unchanged and the caller
                // (binder thread BR_DEAD_BINDER arm) can surface the
                // error without losing the obituary.
                thread_state::clear_death_notification(self.handle())?;
            }

            let snapshot = std::mem::take(&mut *recipients);

            // `Release` so that lock-free `submit_transact` Acquire-loads
            // observing `true` see all writes that happened-before
            // (matching C++'s `mAlive = 0; ... mObitsSent = 1` pattern,
            // where the mutex unlock publishes the writes).
            self.obituary_sent.store(true, Ordering::Release);

            snapshot
        };

        // Callbacks first — these are the user-visible contract.
        // Dispatching before the flush below ensures a transient
        // ioctl failure cannot swallow death notifications.
        for weak in &recipients_snapshot {
            if let Some(recipient) = weak.upgrade() {
                recipient.binder_died(who);
            }
            // Dead `Weak`s are dropped with the snapshot at scope end —
            // the source vector was already cleared by `mem::take`.
        }

        // Flush the queued BC_CLEAR_DEATH_NOTIFICATION outside the
        // lock. If this fails, the command remains in out_parcel and
        // will be sent by `release_obituary_pin`'s phase-2 flush in
        // the same BR_DEAD_BINDER arm — kernel ordering is preserved.
        if !recipients_snapshot.is_empty() {
            thread_state::flush_commands()?;
        }

        Ok(())
    }

    pub fn dump<F: IntoRawFd>(&self, fd: F, args: &[String]) -> Result<()> {
        let mut send = Parcel::new();
        let obj = flat_binder_object::new_with_fd(fd.into_raw_fd(), true);
        send.write_object(&obj, true)?;

        send.write::<i32>(&(args.len() as i32))?;
        for arg in args {
            send.write(arg)?;
        }
        self.submit_transact(DUMP_TRANSACTION, &send, FLAG_CLEAR_BUF)?;
        Ok(())
    }
}

impl Debug for ProxyHandle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Inner")
            .field("handle", &self.handle)
            .field("descriptor", &self.descriptor)
            .field("stability", &self.stability)
            .field("obituary_sent", &self.obituary_sent)
            .finish()
    }
}

impl PartialEq for ProxyHandle {
    fn eq(&self, other: &Self) -> bool {
        self.handle() == other.handle()
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        // The cache pin's BC_INCREFS keeps `binder_ref(handle).weak >= 1`,
        // so this BC_RELEASE is safe regardless of concurrent lookups: the
        // kernel slot is alive on entry, and a future lookup that arrives
        // after the strong count returns to 0 will resurrect a fresh
        // `Arc<ProxyHandle>` via slow-path case (b).
        if let Err(err) = thread_state::dec_strong_handle(self.handle) {
            log::error!(
                "BC_RELEASE for handle {} failed during Drop: {err:?}",
                self.handle
            );
        }
    }
}

impl IBinder for ProxyHandle {
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        // 1. Check cache (read lock). See `ExtensionCache` doc for why
        //    the common case caches strong and only the self-cycle case
        //    caches weak. Sub-cases:
        //      - NotQueried: fall through to remote query.
        //      - Queried(None): authoritatively no extension; return None.
        //      - Queried(Some(Strong(s))): clone and return — no kernel
        //        round trip, no Arc drop on the extension proxy.
        //      - Queried(Some(Weak(w))): self-cycle case; upgrade the
        //        weak (always succeeds while the user holds the parent).
        //        If the parent itself is mid-Drop and the weak is
        //        already dangling, fall through to a fresh remote query
        //        — defensive, this branch should be unreachable in
        //        normal use.
        {
            let cached = self.extension.read().expect("Extension lock poisoned");
            match &*cached {
                ExtensionCache::NotQueried => {}
                ExtensionCache::Queried(None) => return Ok(None),
                ExtensionCache::Queried(Some(CachedExtension::Strong(s))) => {
                    return Ok(Some(s.clone()));
                }
                ExtensionCache::Queried(Some(CachedExtension::Weak(w))) => {
                    if let Ok(strong) = w.upgrade() {
                        return Ok(Some(strong));
                    }
                    // Stale self-cycle weak; fall through to re-query.
                }
            }
        }

        // 2. Remote query (EXTENSION_TRANSACTION)
        let data = Parcel::new();
        let ext: Option<SIBinder> = match self.submit_transact(EXTENSION_TRANSACTION, &data, 0) {
            Ok(Some(mut reply)) => match DeserializeOption::deserialize_option(&mut reply) {
                Ok(ext) => ext,
                Err(_) => return Ok(None),
            },
            _ => return Ok(None),
        };

        // 3. Classify and cache. Strong unless the extension's handle
        //    aliases this proxy's own handle (see `ExtensionCache` doc).
        let entry = ext.as_ref().map(|sib| self.classify_extension(sib));
        let mut cache = self.extension.write().expect("Extension lock poisoned");
        *cache = ExtensionCache::Queried(entry);
        Ok(ext)
    }

    fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        let entry = self.classify_extension(extension);
        let mut ext = self.extension.write().expect("Extension lock poisoned");
        *ext = ExtensionCache::Queried(Some(entry));
        Ok(())
    }

    /// Register a death notification for this object.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Acquire the lock FIRST, then check `obituary_sent` — same
        // ordering as C++ `BpBinder::linkToDeath` (BpBinder.cpp:420
        // `if (!mObitsSent)` runs inside `AutoMutex _l(mLock)`).
        // Checking the flag outside the lock would leave a window where
        // `send_obituary` sets the flag and drains recipients between
        // our check and our `recipients.write()` acquisition, causing a
        // recipient registered after death to never fire.
        let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        if recipients.is_empty() {
            thread_state::request_death_notification(self.handle())?;
            thread_state::flush_commands()?;
        }
        recipients.push(recipient);
        Ok(())
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Acquire the lock FIRST, then check `obituary_sent` — same
        // ordering as C++ `BpBinder::unlinkToDeath` (BpBinder.cpp:456
        // `if (mObitsSent)` runs inside `AutoMutex _l(mLock)`).
        let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        recipients.retain(|r| !sync::Weak::ptr_eq(r, &recipient));
        if recipients.is_empty() {
            thread_state::clear_death_notification(self.handle())?;
            thread_state::flush_commands()?;
        }
        Ok(())
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        thread_state::ping_binder(self.handle())
    }

    fn stability(&self) -> Stability {
        self.stability
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        None
    }

    fn descriptor(&self) -> &str {
        self.descriptor()
    }

    fn is_remote(&self) -> bool {
        true
    }

    // Proxy ref-count methods are no-ops under the cache-pin model.
    //
    // Kernel strong refs are owned 1-per-`Arc<ProxyHandle>` (acquired in
    // `new_acquired`, released in `Drop`). Kernel weak refs are owned by
    // the process-wide cache pin in `ProcessState::handle_to_proxy`. User-
    // side `SIBinder` and `WIBinder` clone/drop is pure `Arc::clone` /
    // `sync::Weak::clone` of the trait-object Arc — no kernel commands.

    fn inc_strong(&self, _strong: &SIBinder) -> Result<()> {
        Ok(())
    }

    fn attempt_inc_strong(&self) -> bool {
        // Unreachable on a proxy: every legitimate caller of
        // `IBinder::attempt_inc_strong` for a proxy either already holds
        // an `Arc<ProxyHandle>` (so the question is moot) or wants
        // "atomically promote a weak ref to a strong ref" semantics — now
        // covered by `Weak<I>::upgrade()` (which uses Rust's
        // `sync::Weak::upgrade` CAS).
        debug_assert!(
            false,
            "attempt_inc_strong called on ProxyHandle — should be unreachable \
             (use Weak<I>::upgrade instead)"
        );
        // Release-build fallback: the cache pin keeps the kernel slot
        // alive, so the legacy "succeed" contract is upheld for any
        // vestigial caller that slips through.
        true
    }

    fn dec_strong(&self, _strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        Ok(())
    }

    fn inc_weak(&self, _weak: &WIBinder) -> Result<()> {
        Ok(())
    }

    fn dec_weak(&self) -> Result<()> {
        Ok(())
    }
}

pub trait Proxy: Sized + Interface {
    /// The Binder interface descriptor string.
    ///
    /// This string is a unique identifier for a Binder interface, and should be
    /// the same between all implementations of that interface.
    fn descriptor() -> &'static str;

    /// Create a new interface from the given proxy, if it matches the expected
    /// type of this interface.
    fn from_binder(binder: SIBinder) -> Option<Self>;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Construct a synthetic `ProxyHandle` with the given `obituary_sent`
    /// initial value, skipping `new_acquired`'s `BC_ACQUIRE` (no
    /// ProcessState/binderfs needed). Caller must `mem::forget` the
    /// returned `Arc` to suppress `Drop`'s `BC_RELEASE`.
    fn synthetic_proxy(obituary_sent: bool) -> Arc<ProxyHandle> {
        Arc::new(ProxyHandle {
            handle: 1,
            descriptor: "test".to_string(),
            stability: Stability::Local,
            obituary_sent: AtomicBool::new(obituary_sent),
            recipients: RwLock::new(Vec::new()),
            extension: RwLock::new(ExtensionCache::NotQueried),
        })
    }

    /// No-op `DeathRecipient` for tests that need a `Weak<dyn ...>` to
    /// pass into `link_to_death` / `unlink_to_death`.
    struct NoopRecipient;
    impl DeathRecipient for NoopRecipient {
        fn binder_died(&self, _who: &WIBinder) {}
    }

    fn noop_recipient_weak() -> sync::Weak<dyn DeathRecipient> {
        let arc: Arc<dyn DeathRecipient> = Arc::new(NoopRecipient);
        Arc::downgrade(&arc)
    }

    #[test]
    fn test_proxy_handle_debug() {
        let handle = synthetic_proxy(false);
        assert_eq!(handle.handle(), 1);
        assert_eq!(handle.descriptor(), "test");

        assert!(handle.as_transactable().is_none());
        assert!(handle.is_remote());

        let debug_str = format!("{handle:?}");
        assert_eq!(
            debug_str,
            "Inner { handle: 1, descriptor: \"test\", stability: Local, obituary_sent: false }"
        );

        std::mem::forget(handle);
    }

    /// `submit_transact` must short-circuit with `DeadObject` when
    /// `obituary_sent` is true — matches C++ `BpBinder::transact`'s
    /// `if (mAlive)` early-exit (BpBinder.cpp:337). The fast-fail path
    /// touches no thread_state IPC, so this test runs without
    /// ProcessState init.
    #[test]
    fn test_submit_transact_fast_fails_when_obituary_sent() {
        let handle = synthetic_proxy(true);
        let parcel = Parcel::new();
        let result = handle.submit_transact(0, &parcel, 0);
        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject, got {result:?}"
        );
        std::mem::forget(handle);
    }

    /// `link_to_death` must reject after obituary — matches C++
    /// `BpBinder::linkToDeath` (BpBinder.cpp:420). The lock-protected
    /// check pattern means the rejection happens AFTER the recipients
    /// write lock is acquired, but no IPC is reached.
    #[test]
    fn test_link_to_death_returns_dead_object_after_obituary() {
        let handle = synthetic_proxy(true);
        let weak_recipient = noop_recipient_weak();
        let result = handle.link_to_death(weak_recipient);
        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject, got {result:?}"
        );
        std::mem::forget(handle);
    }

    /// `unlink_to_death` must reject after obituary — matches C++
    /// `BpBinder::unlinkToDeath` (BpBinder.cpp:456).
    #[test]
    fn test_unlink_to_death_returns_dead_object_after_obituary() {
        let handle = synthetic_proxy(true);
        let weak_recipient = noop_recipient_weak();
        let result = handle.unlink_to_death(weak_recipient);
        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject, got {result:?}"
        );
        std::mem::forget(handle);
    }

    /// Verifies `ExtensionCache::Queried` admits both a strong-cache
    /// variant (common case) and a weak-cache variant (self-cycle
    /// case) at the type level. The discrimination protects against
    /// regressing to either extreme — a strong-only cache would
    /// reintroduce the self-referencing `Arc<ProxyHandle>` cycle, and
    /// a weak-only cache would reintroduce the
    /// `BC_RELEASE`/`BC_ACQUIRE` thrash that produced
    /// `BR_FAILED_REPLY` ("cannot find target node") under stress.
    #[test]
    fn test_extension_cache_variant_holds_dual_modes() {
        // Compile-time check: the payload type matches the documented
        // shape `Option<CachedExtension>`, and `CachedExtension` exposes
        // both `Strong(SIBinder)` and `Weak(WIBinder)` constructors.
        // Wrong inner types would fail the type-checked bindings; a
        // missing variant would fail the `fn _exhaust` exhaustiveness
        // check; a non-`Option` payload would fail the `_typed` binding.
        let none_cache = ExtensionCache::Queried(None);
        let ExtensionCache::Queried(payload) = &none_cache else {
            unreachable!("constructed Queried, must match Queried")
        };
        let _typed: &Option<CachedExtension> = payload;

        // Exhaustiveness gate: this fn fails to compile if a future
        // patch removes either variant (or adds a third without
        // updating callers).
        fn _exhaust(entry: &CachedExtension) -> &'static str {
            match entry {
                CachedExtension::Strong(_) => "strong",
                CachedExtension::Weak(_) => "weak",
            }
        }
    }
}

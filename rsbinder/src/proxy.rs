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
    /// Set once when `send_obituary` runs to publish "this proxy is
    /// dead" to all observers.
    ///
    /// Three call sites read this flag with three different orderings.
    /// All three are correct — a future reader who sees `Relaxed` and
    /// "fixes" it to `Acquire` would add a fence with no benefit.
    ///
    /// | Call site                       | Lock state             | Ordering | Why                                                                            |
    /// |---------------------------------|------------------------|----------|--------------------------------------------------------------------------------|
    /// | `submit_transact`, `dump`       | none (lock-free)       | `Acquire`| Pairs with `Release` store in `send_obituary` to publish recipients teardown.  |
    /// | `link_to_death`/`unlink_to_death`| inside `recipients` write lock | `Relaxed`| RwLock acquire/release supplies happens-before against `send_obituary`'s store.|
    /// | `send_obituary`                 | inside `recipients` write lock | `Relaxed`| Same — the store's `Release` is for the *lock-free* readers, not the lock-protected ones. |
    ///
    /// In short: the `Acquire`/`Release` pair on `obituary_sent`
    /// itself is what protects the **lock-free** `submit_transact` /
    /// `dump` fast-fail paths. The lock-protected paths get their
    /// happens-before from the surrounding `RwLock` and so the
    /// atomic load can be `Relaxed` there.
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
        // ioctl failure cannot swallow death notifications. Panics
        // inside individual recipients are caught and logged so a
        // single buggy recipient cannot terminate the binder worker
        // thread or starve the remaining recipients.
        self.dispatch_obituary_callbacks(&recipients_snapshot, who);

        // Flush the queued BC_CLEAR_DEATH_NOTIFICATION outside the
        // lock. If this fails, the command remains in out_parcel and
        // will be sent by `release_obituary_pin`'s phase-2 flush in
        // the same BR_DEAD_BINDER arm — kernel ordering is preserved.
        if !recipients_snapshot.is_empty() {
            thread_state::flush_commands()?;
        }

        Ok(())
    }

    /// Invoke `binder_died` on every live recipient in `snapshot`,
    /// isolating panics so a single buggy recipient cannot abort the
    /// binder worker thread or starve the remaining recipients.
    ///
    /// Guarantees:
    /// - One recipient panicking does not prevent later recipients from
    ///   receiving `binder_died`.
    /// - The binder worker thread continues running after a recipient
    ///   panic; the panic is logged and discarded.
    ///
    /// Not guaranteed: the panicking recipient's own internal state
    /// consistency. `AssertUnwindSafe` is a deliberate assertion that
    /// rsbinder does not attempt to repair user state across the
    /// unwind boundary — `recipients_snapshot` was already detached
    /// from `self.recipients` and `obituary_sent` was already
    /// published before this call, so rsbinder's own invariants are
    /// unaffected.
    ///
    /// `panic = "abort"` builds turn this guard into a no-op (the
    /// process aborts on any panic, including from a buggy
    /// recipient). Documented on the `DeathRecipient` trait.
    fn dispatch_obituary_callbacks(
        &self,
        snapshot: &[sync::Weak<dyn DeathRecipient>],
        who: &WIBinder,
    ) {
        for weak in snapshot {
            // Dead `Weak`s are dropped with the snapshot at scope end —
            // the source vector was already cleared by `mem::take`.
            let Some(recipient) = weak.upgrade() else {
                continue;
            };
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                recipient.binder_died(who);
            }));
            if let Err(payload) = result {
                let msg = payload
                    .downcast_ref::<&'static str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("<non-string panic payload>");
                log::error!(
                    "DeathRecipient panicked during binder_died for handle {:X}: {msg}",
                    self.handle,
                );
            }
        }
    }

    pub fn dump<F: IntoRawFd>(&self, fd: F, args: &[String]) -> Result<()> {
        // Fast-fail BEFORE consuming the fd. `submit_transact` would
        // also short-circuit on `obituary_sent` (PR #104 §1), but by
        // the time we reach it, `fd.into_raw_fd()` has already
        // detached the raw fd from `F`'s RAII; an early `Err` from
        // `submit_transact` would then leak the fd. Mirroring the
        // `submit_transact` Acquire-load here lets `F` drop naturally
        // (closing the fd) when the proxy is already dead. The non-
        // fast-fail error paths (parcel-write failures, in-`transact`
        // errors after the kernel sees the fd) still exhibit the
        // pre-existing leak shape — see FOLLOW_UP_PR_104.md Item 4
        // "Follow-up scope" for the wider fix.
        if self.obituary_sent.load(Ordering::Acquire) {
            return Err(StatusCode::DeadObject);
        }
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

    fn set_extension(&self, _extension: &SIBinder) -> Result<()> {
        // `set_extension` is a server-side operation: a service
        // publishes its extension binder for clients to discover via
        // `get_extension()`. Calling it on a client proxy used to
        // succeed silently, caching locally without telling the
        // remote service — and after PR #104's §4 scope-down the
        // local cache holds a strong `Arc<dyn IBinder>` for the
        // parent's lifetime, silently pinning an unrelated binder.
        // Reject with `InvalidOperation`, matching the default trait
        // impl in `binder.rs`. In-tree callers operate on native
        // `Binder`, not `ProxyHandle`, so this reject is safe (see
        // `tests/src/test_client.rs`, `tests/src/bin/test_service.rs`,
        // `tests/src/bin/test_service_async.rs`).
        Err(StatusCode::InvalidOperation)
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
        // Reject a recipient whose strong count is already zero — a
        // common mistake when the caller drops the
        // `Arc<dyn DeathRecipient>` before passing the weak. Without
        // this check `send_obituary` would silently skip the
        // recipient via `weak.upgrade() == None`, leaving the user
        // with the false impression that they successfully
        // registered. Placed before `request_death_notification` so
        // a dead weak never consumes a kernel subscription. Does
        // *not* protect against the recipient's `Arc` being dropped
        // *between* `link_to_death` returning and `binder_died`
        // firing — that's a legitimate use of `Weak` semantics.
        if recipient.upgrade().is_none() {
            return Err(StatusCode::BadValue);
        }
        if recipients.is_empty() {
            // Match C++ `BpBinder::linkToDeath` (BpBinder.cpp:415-434):
            // queue `BC_REQUEST_DEATH_NOTIFICATION` and best-effort
            // flush. The parcel-write of `BC_REQUEST_DEATH_NOTIFICATION`
            // itself can fail (out_parcel corruption — rare, e.g. OOM)
            // and propagates because that signals state corruption,
            // not a driver round-trip issue. The subsequent
            // `flush_commands` is intentionally **not** propagated —
            // ignoring it (a) matches Android's symmetric behavior
            // (C++ ignores `flushCommands`'s return value) and (b)
            // closes the prior leak window where a flush failure
            // skipped `recipients.push` and left the kernel with a
            // subscription that no user-side recipient could service.
            thread_state::request_death_notification(self.handle())?;
            let _ = thread_state::flush_commands();
        }
        recipients.push(recipient);
        Ok(())
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    ///
    /// Returns `Err(StatusCode::NameNotFound)` if no matching
    /// recipient is registered. Removes only the first matching
    /// entry, mirroring C++ `BpBinder::unlinkToDeath`'s
    /// `mObituaries->removeAt(i); return NO_ERROR;` (BpBinder.cpp:443-484)
    /// — a user that registered the same recipient twice and unlinks
    /// once expects one callback to remain.
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Acquire the lock FIRST, then check `obituary_sent` — same
        // ordering as C++ `BpBinder::unlinkToDeath` (BpBinder.cpp:456
        // `if (mObitsSent)` runs inside `AutoMutex _l(mLock)`).
        let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        // Single-position removal (O(n), order-preserving). Matches
        // C++ `removeAt(i)` semantics; a `retain` here would remove
        // every duplicate registration of the same recipient and
        // silently drop the user's remaining subscription. The
        // `clear_death_notification` IPC fires only when this call
        // actually transitions the list from non-empty to empty —
        // unlinking from an already-empty list (NameNotFound path)
        // must not queue a `BC_CLEAR_DEATH_NOTIFICATION` for a
        // subscription that was never requested.
        let Some(i) = recipients
            .iter()
            .position(|r| sync::Weak::ptr_eq(r, &recipient))
        else {
            return Err(StatusCode::NameNotFound);
        };
        recipients.remove(i);
        if recipients.is_empty() {
            // Symmetric with `link_to_death`: queue
            // `BC_CLEAR_DEATH_NOTIFICATION`, propagate parcel-write
            // failure (state corruption), ignore `flush_commands`'s
            // return value (best-effort, matches C++).
            thread_state::clear_death_notification(self.handle())?;
            let _ = thread_state::flush_commands();
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

    /// Build a `(strong, weak)` recipient pair. The caller binds the
    /// returned `Arc` for the test's duration; dropping it would
    /// turn the returned `Weak` into a dangling reference that
    /// `Weak::upgrade()` and the production-side liveness check
    /// would treat as already-dead.
    ///
    /// Older tests used `noop_recipient_weak()` which returned a
    /// dangling weak directly. That was benign for fast-fail tests
    /// (the obituary check fired first) but masked the upgrade
    /// liveness check added in Item 8 — a future test added against
    /// the dangling form would fail mysteriously.
    fn live_recipient_pair() -> (Arc<dyn DeathRecipient>, sync::Weak<dyn DeathRecipient>) {
        let arc: Arc<dyn DeathRecipient> = Arc::new(NoopRecipient);
        let weak = Arc::downgrade(&arc);
        (arc, weak)
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
        let (_arc, weak_recipient) = live_recipient_pair();
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
        let (_arc, weak_recipient) = live_recipient_pair();
        let result = handle.unlink_to_death(weak_recipient);
        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject, got {result:?}"
        );
        std::mem::forget(handle);
    }

    /// Registering the same recipient twice and unlinking once must
    /// remove only one entry — the user's remaining registration is
    /// silently lost if `unlink_to_death` removes every match (the
    /// `Vec::retain` bug fixed here). Mirrors C++
    /// `BpBinder::unlinkToDeath` returning after a single
    /// `mObituaries->removeAt(i)`. The recipients vector is
    /// populated directly so the test does not require ProcessState
    /// init for `link_to_death`'s `request_death_notification` IPC.
    #[test]
    fn test_unlink_to_death_removes_only_one_match() {
        let proxy = synthetic_proxy(false);
        let (_arc, weak) = live_recipient_pair();
        {
            let mut recipients = proxy.recipients.write().expect("recipients lock");
            recipients.push(weak.clone());
            recipients.push(weak.clone());
        }

        let result = proxy.unlink_to_death(weak.clone());
        assert!(matches!(result, Ok(())), "expected Ok, got {result:?}");

        let recipients = proxy.recipients.read().expect("recipients lock");
        assert_eq!(
            recipients.len(),
            1,
            "exactly one duplicate must remain (single-position remove)"
        );
        assert!(
            sync::Weak::ptr_eq(&recipients[0], &weak),
            "remaining entry must be the same recipient that was registered twice"
        );
        drop(recipients);
        std::mem::forget(proxy);
    }

    /// `unlink_to_death` on an empty recipients list must return
    /// `NameNotFound` and must not queue a
    /// `BC_CLEAR_DEATH_NOTIFICATION` to the kernel — there is no
    /// subscription to clear. The position check happens before any
    /// IPC, so this test runs without ProcessState init.
    #[test]
    fn test_unlink_to_death_empty_list_returns_name_not_found() {
        let proxy = synthetic_proxy(false);
        let (_arc, weak) = live_recipient_pair();

        let result = proxy.unlink_to_death(weak);
        assert!(
            matches!(result, Err(StatusCode::NameNotFound)),
            "expected NameNotFound, got {result:?}"
        );
        assert!(
            proxy.recipients.read().expect("recipients lock").is_empty(),
            "recipients vec must remain empty"
        );
        std::mem::forget(proxy);
    }

    /// Locks down the documented staleness behavior of the strong
    /// extension cache: when the *extension* (not the parent) is
    /// obituary'd, the parent's `get_extension()` keeps returning
    /// the cached (now dead) `SIBinder`. The cache is **not**
    /// auto-invalidated. IPC through the dead extension fast-fails
    /// with `DeadObject` via `submit_transact`'s obituary check, so
    /// the dead reference is well-behaved — but a server that
    /// re-publishes a fresh extension is invisible to the client
    /// until the parent is dropped and re-acquired. Matches Android
    /// C++ `BpBinder` semantics. A future "auto-invalidate" change
    /// would deliberately flip this assertion.
    #[test]
    fn test_get_extension_strong_cache_does_not_auto_invalidate_on_dead_extension() {
        let ext_proxy = synthetic_proxy(true); // extension already obituary'd
        let ext_arc_dyn: Arc<dyn IBinder> = ext_proxy.clone();
        let ext_sibinder = SIBinder::from_arc(ext_arc_dyn);

        let parent = synthetic_proxy(false);
        {
            let mut cache = parent.extension.write().expect("Extension lock poisoned");
            *cache = ExtensionCache::Queried(Some(CachedExtension::Strong(ext_sibinder)));
        }

        // get_extension returns the cached (dead) extension via the
        // cache-hit path — no remote query attempted.
        let returned = parent
            .get_extension()
            .expect("get_extension")
            .expect("cache hit should return Some");

        // The returned SIBinder is the same dead ProxyHandle. IPC
        // through it must fast-fail.
        let returned_proxy = (*returned).as_proxy().expect("extension is a proxy");
        assert_eq!(returned_proxy.handle(), ext_proxy.handle());
        let parcel = Parcel::new();
        assert!(
            matches!(
                returned_proxy.submit_transact(0, &parcel, 0),
                Err(StatusCode::DeadObject)
            ),
            "calls through cached dead extension must fast-fail with DeadObject"
        );

        // Cache must remain Strong — staleness is the documented
        // contract. A regression to "auto-invalidate" would flip the
        // variant to NotQueried (or remove the entry) and trip this
        // assertion.
        let cache = parent.extension.read().expect("Extension lock poisoned");
        assert!(
            matches!(
                *cache,
                ExtensionCache::Queried(Some(CachedExtension::Strong(_)))
            ),
            "cache must remain Strong after get_extension on a dead extension"
        );
        drop(cache);

        // Drop order: `returned` first so its strong count decrement
        // doesn't race with cache teardown. The cache still holds
        // one strong via the parent — `parent` is then forgotten so
        // the synthetic ProxyHandle's `BC_RELEASE` Drop never runs.
        drop(returned);
        std::mem::forget(parent);
        std::mem::forget(ext_proxy);
    }

    /// `link_to_death` must reject a recipient whose strong count
    /// is already zero with `BadValue`, **before** queuing
    /// `BC_REQUEST_DEATH_NOTIFICATION`. Otherwise the kernel would
    /// register a subscription that no user-side recipient can ever
    /// service (silent registration of a dead recipient is a
    /// misleading API). The recipients vec must remain unchanged.
    #[test]
    fn test_link_to_death_rejects_already_dead_weak() {
        let proxy = synthetic_proxy(false); // obituary not sent

        // Build a Weak whose strong count starts at zero.
        let arc: Arc<dyn DeathRecipient> = Arc::new(NoopRecipient);
        let dead_weak = Arc::downgrade(&arc);
        drop(arc);
        assert!(
            dead_weak.upgrade().is_none(),
            "fixture sanity: weak must be dangling"
        );

        let result = proxy.link_to_death(dead_weak);
        assert!(
            matches!(result, Err(StatusCode::BadValue)),
            "expected BadValue, got {result:?}"
        );
        assert!(
            proxy.recipients.read().expect("recipients lock").is_empty(),
            "dead-weak rejection must not push a recipient"
        );
        std::mem::forget(proxy);
    }

    /// `ProxyHandle::set_extension` must reject with
    /// `InvalidOperation` — the operation is server-side only and a
    /// proxy has no way to inform the remote service. Silent
    /// success would (a) leave the remote unaware of the new
    /// extension and (b) after PR #104's §4 scope-down, pin an
    /// unrelated `Arc<dyn IBinder>` for the parent's lifetime via
    /// the strong-cache common case. The cache must remain
    /// `NotQueried` after the rejected call.
    #[test]
    fn test_set_extension_on_proxy_rejects_with_invalid_operation() {
        let proxy = synthetic_proxy(false);
        let ext = SIBinder::new(Arc::new(MockBinder)).expect("SIBinder::new");

        let result = proxy.set_extension(&ext);
        assert!(
            matches!(result, Err(StatusCode::InvalidOperation)),
            "expected InvalidOperation, got {result:?}"
        );

        // Cache must not have been mutated. Constructing the
        // synthetic proxy started in NotQueried; verify it stayed
        // there.
        let cache = proxy.extension.read().expect("Extension lock poisoned");
        assert!(
            matches!(*cache, ExtensionCache::NotQueried),
            "extension cache must remain NotQueried after a rejected set_extension"
        );
        drop(cache);
        std::mem::forget(proxy);
    }

    /// `dump` must fast-fail when the proxy is already obituary'd,
    /// **before** calling `fd.into_raw_fd()` — otherwise the raw fd
    /// is detached from `F`'s RAII and an early `Err` from
    /// `submit_transact` leaks the fd. The synthetic `DropFlag`
    /// implements `IntoRawFd` so the function's bound is satisfied,
    /// but its `Drop` records observation; the assertion checks that
    /// the fast-fail path leaves the fd to RAII (Drop fires) rather
    /// than consuming it.
    #[test]
    fn test_dump_fast_fails_and_drops_fd_when_obituary_sent() {
        use std::os::fd::RawFd;

        struct DropFlag {
            dropped: Arc<AtomicBool>,
        }
        impl std::os::fd::IntoRawFd for DropFlag {
            fn into_raw_fd(self) -> RawFd {
                // In the real success path the kernel takes ownership
                // of the raw fd, so suppress Drop. The fast-fail path
                // must NOT reach this method — the test would silently
                // miss the Drop assertion below if it did.
                std::mem::forget(self);
                -1
            }
        }
        impl Drop for DropFlag {
            fn drop(&mut self) {
                self.dropped.store(true, Ordering::SeqCst);
            }
        }

        let proxy = synthetic_proxy(true); // obituary_sent
        let dropped = Arc::new(AtomicBool::new(false));
        let result = proxy.dump(
            DropFlag {
                dropped: dropped.clone(),
            },
            &[],
        );

        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject fast-fail, got {result:?}"
        );
        assert!(
            dropped.load(Ordering::SeqCst),
            "DropFlag's Drop must fire — fast-fail path must not call into_raw_fd"
        );

        std::mem::forget(proxy);
    }

    /// Unlinking a recipient that was never registered (while
    /// another, different recipient *is* registered) must return
    /// `NameNotFound` and leave the existing registration
    /// untouched — `Vec::retain` would also do nothing in this case
    /// but with the buggy "remove all matches" semantic, a future
    /// regression that re-introduces it would fail this assertion
    /// only via the registered-twice variant. Belt-and-suspenders.
    #[test]
    fn test_unlink_to_death_unregistered_returns_name_not_found() {
        let proxy = synthetic_proxy(false);
        let (_a_arc, a_weak) = live_recipient_pair();
        let (_b_arc, b_weak) = live_recipient_pair();
        {
            let mut recipients = proxy.recipients.write().expect("recipients lock");
            recipients.push(a_weak.clone());
        }

        let result = proxy.unlink_to_death(b_weak);
        assert!(
            matches!(result, Err(StatusCode::NameNotFound)),
            "expected NameNotFound, got {result:?}"
        );

        let recipients = proxy.recipients.read().expect("recipients lock");
        assert_eq!(recipients.len(), 1, "registered recipient must survive");
        assert!(
            sync::Weak::ptr_eq(&recipients[0], &a_weak),
            "surviving entry must be the originally registered recipient"
        );
        drop(recipients);
        std::mem::forget(proxy);
    }

    /// Minimal native `IBinder` impl used to build a `WIBinder` for
    /// tests that need a `who` argument but don't care about the
    /// binder's identity. `SIBinder::downgrade` takes the Native
    /// branch for this type, so no `ProcessState` init is required.
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

    /// A panicking recipient must not abort the binder worker thread
    /// or starve subsequent recipients in the same `send_obituary`
    /// snapshot. Drives `dispatch_obituary_callbacks` directly with a
    /// two-element snapshot — the panicking entry is placed first so a
    /// regression that drops the `catch_unwind` guard would unwind
    /// past the loop and leave the counting recipient untouched, which
    /// the assertion would then catch. Captured panic output is
    /// printed to stderr by the default panic hook; cargo test buffers
    /// it per-test so it only surfaces if this test itself fails.
    #[test]
    fn test_dispatch_obituary_callbacks_isolates_panic() {
        use std::sync::Mutex;

        struct PanickingRecipient;
        impl DeathRecipient for PanickingRecipient {
            fn binder_died(&self, _who: &WIBinder) {
                panic!("simulated recipient panic");
            }
        }

        struct CountingRecipient {
            count: Arc<Mutex<u32>>,
        }
        impl DeathRecipient for CountingRecipient {
            fn binder_died(&self, _who: &WIBinder) {
                *self.count.lock().expect("count lock") += 1;
            }
        }

        let panic_arc: Arc<dyn DeathRecipient> = Arc::new(PanickingRecipient);
        let count = Arc::new(Mutex::new(0u32));
        let counting_arc: Arc<dyn DeathRecipient> = Arc::new(CountingRecipient {
            count: count.clone(),
        });

        let snapshot: Vec<sync::Weak<dyn DeathRecipient>> =
            vec![Arc::downgrade(&panic_arc), Arc::downgrade(&counting_arc)];

        let mock_strong = SIBinder::new(Arc::new(MockBinder)).expect("SIBinder::new");
        let who = SIBinder::downgrade(&mock_strong);

        let proxy = synthetic_proxy(false);
        proxy.dispatch_obituary_callbacks(&snapshot, &who);

        assert_eq!(
            *count.lock().expect("count lock"),
            1,
            "counting recipient must fire after panicking recipient \
             (catch_unwind guard regression)"
        );

        std::mem::forget(proxy);
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

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Client proxy for remote binder services.
//!
//! This module provides the client-side infrastructure for communicating with
//! remote binder services, including proxy objects that represent remote services
//! and handle transaction routing and lifecycle management.
//!
//! # Death notifications
//!
//! `ProxyHandle` follows C++ `BpBinder` (`BpBinder.cpp`) for the recipient
//! list and the obituary:
//!
//! - `link_to_death` and `unlink_to_death` take the `recipients` write lock
//!   before reading `obituary_sent`, as `BpBinder::linkToDeath` /
//!   `unlinkToDeath` read `mObitsSent` inside `AutoMutex _l(mLock)`. Checking
//!   the flag outside the lock would leave a window where `send_obituary`
//!   sets the flag and drains the recipients between the check and the lock,
//!   so a recipient registered after death would never fire.
//! - `BC_REQUEST_DEATH_NOTIFICATION` is queued only when the list goes from
//!   empty to non-empty, and `BC_CLEAR_DEATH_NOTIFICATION` only when an unlink
//!   takes it from non-empty to empty; unlinking from an already-empty list
//!   (the `NameNotFound` path) queues nothing for a subscription that was
//!   never requested. A failed parcel write of either command propagates, as
//!   it signals `out_parcel` corruption (rare, e.g. OOM) rather than a driver
//!   round-trip problem. The following `flush_commands` result is ignored,
//!   as C++ ignores `flushCommands`; propagating it would skip
//!   `recipients.push` and leave the kernel with a subscription that no
//!   recipient can service.
//! - `link_to_death` rejects a recipient whose strong count is already zero
//!   (`BadValue`) — a common mistake when the caller drops the
//!   `Arc<dyn DeathRecipient>` before passing the weak, which `send_obituary`
//!   would otherwise skip silently. The check runs before
//!   `request_death_notification`, so a dead weak never consumes a kernel
//!   subscription. It does not cover a recipient dropped after
//!   `link_to_death` returns; that is ordinary `Weak` semantics.
//! - `unlink_to_death` removes only the first matching entry
//!   (order-preserving, C++ `removeAt(i)`); a `retain` would drop every
//!   duplicate registration and silently remove the user's remaining
//!   subscription.
//!
//! `send_obituary` mirrors `BpBinder::sendObituary`:
//!
//! 1. `obituary_sent` is read and set only under the `recipients` lock, so a
//!    racing `link_to_death` cannot push a recipient into the just-drained
//!    list, where it would never fire.
//! 2. The list is detached under the lock and the callbacks run after the lock
//!    is released, so a callback may re-enter `link_to_death` /
//!    `unlink_to_death` on the same proxy without deadlocking.
//! 3. A second `send_obituary` (e.g. a spurious double `BR_DEAD_BINDER`) sees
//!    `obituary_sent` and returns, as C++'s `if (mObitsSent) return;`.
//! 4. `BC_CLEAR_DEATH_NOTIFICATION` is queued before the list is taken, best
//!    effort: `BR_DEAD_BINDER` is delivered once, so a queueing failure must
//!    not abort the obituary, or the recipients would never hear of the death
//!    and `obituary_sent` would never latch.
//! 5. The `Release` store of `obituary_sent` publishes the teardown to the
//!    lock-free `Acquire` loads in `submit_transact` and `dump` (C++
//!    `mAlive = 0; ... mObitsSent = 1`, published by the mutex unlock). Those
//!    loads fast-fail a call that would only get `BR_DEAD_REPLY`, as
//!    `BpBinder::transact` checks `mAlive` outside `mLock`.
//! 6. Callbacks run before the flush, so a transient ioctl failure cannot
//!    swallow the obituary; a panicking recipient is caught and logged so it
//!    cannot stop the worker thread or starve the remaining recipients. If the
//!    flush fails, the command stays in `out_parcel` and is sent by
//!    `finish_obituary`'s phase-2 flush in the same `BR_DEAD_BINDER` arm,
//!    which preserves kernel ordering.
//!
//! # Reference counts
//!
//! The `IBinder` ref-count methods are no-ops on a proxy under the cache-pin
//! model. Kernel strong refs are owned one per `Arc<ProxyHandle>` (acquired
//! in `new_acquired`, released in `Drop`); the kernel weak ref is the cache pin
//! (`process_state::HandlePin`) shared by the entry's proxies and `WIBinder`s.
//! User-side clone and drop sends no kernel command, except the last proxy drop (`BC_RELEASE`)
//! and the pin's last drop.
//!
//! The pin's `BC_INCREFS` keeps `binder_ref(handle).weak >= 1`, so the
//! `BC_RELEASE` in `Drop` is safe regardless of concurrent lookups: the kernel
//! slot is alive on entry. `Drop` clears a still-linked death subscription, then
//! sends `BC_RELEASE` (AOSP `onLastStrongRef`). Once the strong count returns to 0, only a fresh
//! wire delivery (e.g. servicemanager `checkService`) re-establishes a
//! transactable strong ref; until one does, an in-process `WIBinder::upgrade()`
//! returns `DeadObject`, and after it, the revived proxy.
//!
//! # Operations without meaning on a proxy
//!
//! `IBinder::attempt_inc_strong` is meaningless on a proxy: a caller either
//! already holds an `Arc<ProxyHandle>` or wants weak-to-strong promotion,
//! which `Weak<I>::upgrade()` provides. The trait is public and `SIBinder`
//! derefs to it, so external code can still reach it; the proxy logs a
//! warning and honours the "succeed" contract instead of asserting.
//!
//! `set_extension` is a server-side operation (a service publishes its
//! extension for clients to find via `get_extension`). A proxy cannot inform
//! the remote, and caching the binder locally would pin an unrelated
//! `Arc<dyn IBinder>` for the parent's lifetime, so it returns
//! `InvalidOperation` like the default trait impl.
//!
//! # Proxy construction
//!
//! `ProxyHandle::new_acquired` allocates the `Arc<ProxyHandle>` and sends one
//! `BC_ACQUIRE`. Its caller must hold the `ProcessState::handle_to_proxy`
//! write lock and a reference to the handle's pin: a fresh one whose
//! `BC_INCREFS` it issued and flushed (sub-case (a)), or the existing entry's
//! pin upgraded under that lock (sub-case (b)). The pin keeps the
//! `binder_ref` slot alive, so this `BC_ACQUIRE` cannot race a concurrent
//! `BC_RELEASE` into a freed slot.
//!
//! # Proxy identity
//!
//! A handle id is unique only while its `binder_ref` slot lives; the kernel
//! may recycle it for a different node afterwards. The handle's pin therefore
//! carries the process-wide generation counter snapshotted when its cache
//! entry was created, and `(handle, generation)` identifies the *node*; that
//! pair is what `WIBinder` equality and `ProxyHandle`'s `PartialEq` compare.
//!
//! The generation is read from the proxy's pin rather than looked up from the proxy
//! cache on demand, because the obituary retires the cache entry *before*
//! dispatching `binder_died`. A cache lookup would answer `None` for exactly
//! the binders a death recipient needs to match: a `downgrade` taken inside
//! the recipient would fall back to the `Native` variant, which compares
//! unequal to the `Proxy` weak the obituary carries and to every weak taken
//! while the binder was alive. A distinct allocation naming the same
//! `(handle, generation)` (what a case-(b) re-creation produces) compares
//! equal. See `SIBinder::downgrade`.
//!
//! # Proxy counting
//!
//! `tracked_uid` is the uid `crate::proxy_count`'s per-uid map charges this
//! proxy to, captured at construction via `thread_state::get_calling_uid`
//! (AOSP `IPCThreadState::getCallingUid()`): the sender uid of the
//! `BR_TRANSACTION` being handled, or this process's own `getuid()` when no
//! incoming transaction is on the stack.
//!
//! `count_acquired` is `true` iff construction reached
//! `proxy_count::on_proxy_create`, so `Drop` owes a matching `on_proxy_drop`.
//! It stays `false` only for test-only construction (`synthetic_proxy`); a
//! failed `inc_strong_handle` returns before any `ProxyHandle` exists.
//! `counted_by_uid` is `true` iff `on_proxy_create` incremented the per-uid
//! map (tracking was enabled at construction). `Drop` decides from this field,
//! not from the live `COUNT_BY_UID_ENABLED` flag, so disabling tracking while
//! the proxy lives cannot desync the count (AOSP `BpBinder::mTrackedUid`).
//! Both are plain `bool`: written once before the `Arc<ProxyHandle>` is
//! shared and read only in `Drop` (`&mut self`); the `Arc` refcount's
//! release/acquire supplies the happens-before.
//!
//! # `obituary_sent` ordering
//!
//! `send_obituary` sets `obituary_sent` once to publish "this proxy is dead".
//! Its three readers use three orderings, and all three are correct: changing
//! the `Relaxed` loads to `Acquire` adds a fence with no effect.
//!
//! | Call site | Lock state | Ordering | Why |
//! |---|---|---|---|
//! | `submit_transact`, `dump` | none (lock-free) | `Acquire` | Pairs with the `Release` store in `send_obituary` that publishes the recipients teardown. |
//! | `link_to_death` / `unlink_to_death` | inside `recipients` write lock | `Relaxed` | The `RwLock` acquire/release orders it against `send_obituary`'s store. |
//! | `send_obituary` | inside `recipients` write lock | `Relaxed` | Same; the store's `Release` serves the lock-free readers only. |
//!
//! # Extension cache
//!
//! The cached extension is held strongly or weakly depending on whether its
//! handle aliases the parent proxy's own handle:
//!
//! - **Strong (common case)**: the extension is a different binder. The parent
//!   holds an `SIBinder`, so the extension's `Arc<ProxyHandle>` lives as long
//!   as the parent. A weak cache would let that `Arc` drop and be re-created
//!   on every `get_extension`, producing a stream of `BC_RELEASE` /
//!   `BC_ACQUIRE` pairs against the kernel `binder_ref`. Under stress the
//!   `binder-linux` driver can lose the `binder_ref → binder_node`
//!   association across that churn and answer the next transaction with
//!   `BR_FAILED_REPLY` ("cannot find target node").
//! - **Weak (self-cycle)**: the extension's handle equals the parent's (a
//!   remote naming itself as its own extension). A strong cache would form an
//!   `Arc<ProxyHandle>` cycle through the parent's own state and the parent
//!   would never drop. The user must hold an external strong ref to the parent
//!   for the extension to be reachable; `weak.upgrade()` then reuses that
//!   `Arc` without cache-pin re-creation, so this case has no
//!   `BC_RELEASE` / `BC_ACQUIRE` churn either.
//! - **Longer cycles are not broken.** Only the self-cycle is detected
//!   (`handle == parent.handle`). A remote that reports A's extension as B and
//!   B's as A leaves two strong caches pointing at each other once both
//!   `get_extension`s have run; neither `ProxyHandle` drops and neither
//!   `BC_RELEASE` is sent. Extension graphs are remote-controlled, so this is
//!   a known limit of the cache.
//!
//! The cache is not invalidated when the extension (not the parent) dies:
//! `get_extension` keeps returning the dead `SIBinder`, whose calls fast-fail
//! with `DeadObject`, and a freshly re-published extension stays invisible
//! until the parent is dropped and re-acquired, as in C++ `BpBinder`.
//!
//! # Recipient panics
//!
//! `dispatch_obituary_callbacks` wraps each `binder_died` in `catch_unwind`, so
//! one panicking recipient neither stops the worker thread nor starves later
//! recipients. `AssertUnwindSafe` asserts that rsbinder does not repair user
//! state across the unwind: the snapshot is already detached from
//! `recipients` and `obituary_sent` already published, so rsbinder's own
//! invariants are unaffected; the panicking recipient's own state is not
//! guaranteed. With `panic = "abort"` the guard does nothing (documented on
//! `DeathRecipient`).

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::mem::ManuallyDrop;
use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{self, Arc, RwLock};

use crate::process_state::HandlePin;
use crate::{binder::*, error::*, parcel::*, parcelable::DeserializeOption, thread_state};

/// Proxy-side extension cache; strong vs weak rule in module doc "Extension cache".
enum ExtensionCache {
    /// Remote query has not been performed yet.
    NotQueried,
    /// Remote query completed; stores the result (Some or None).
    Queried(Option<CachedExtension>),
}

enum CachedExtension {
    /// Common case: extension proxy distinct from the parent proxy.
    Strong(SIBinder),
    /// The extension's handle aliases the parent's; weak avoids an `Arc<ProxyHandle>` self-cycle.
    Weak(WIBinder),
}

/// Handle for a proxy to a remote binder service.
///
/// Owns exactly **one kernel strong ref** (`BC_ACQUIRE` at construction,
/// `BC_RELEASE` on `Drop`). The kernel weak ref that keeps the
/// `binder_ref` slot alive across `strong = 0` windows is the handle's
/// cache pin, which this type shares with the other proxies and proxy
/// `WIBinder`s of the same cache entry; the last of them releases it — see
/// the `process_state` module doc "Proxy cache entry".
pub struct ProxyHandle {
    handle: u32,
    /// Shared `BC_INCREFS`; its generation is this proxy's. See module doc "Proxy identity".
    pin: Arc<HandlePin>,
    descriptor: String,
    /// Uid charged in `proxy_count`'s per-uid map; see module doc "Proxy counting".
    tracked_uid: u32,
    /// `Drop` owes `on_proxy_drop` iff this is set; see module doc "Proxy counting".
    count_acquired: bool,
    /// Per-uid map was incremented at construction; `Drop` reads this, not the live flag.
    counted_by_uid: bool,
    stability: Stability,
    /// Lock-free readers `Acquire`, locked readers `Relaxed`; see the module doc table.
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<sync::Weak<dyn DeathRecipient>>>,
    extension: RwLock<ExtensionCache>,
}

impl ProxyHandle {
    /// Sends `BC_ACQUIRE`; caller holds the proxy-cache lock and `pin`. See module doc.
    pub(crate) fn new_acquired(
        pin: &Arc<HandlePin>,
        descriptor: String,
        stability: Stability,
    ) -> Result<Arc<Self>> {
        let handle = pin.handle();
        // Outside a transaction this is this process's own uid, as in AOSP.
        let tracked_uid = thread_state::get_calling_uid();
        // Kernel ref before `on_proxy_create`, so a failure owes no `on_proxy_drop`.
        thread_state::inc_strong_handle(handle)?;
        let counted_by_uid = crate::proxy_count::on_proxy_create(tracked_uid);
        Ok(Arc::new(Self {
            handle,
            pin: Arc::clone(pin),
            descriptor,
            tracked_uid,
            count_acquired: true,
            counted_by_uid,
            stability,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
            extension: RwLock::new(ExtensionCache::NotQueried),
        }))
    }

    /// Get the underlying binder handle number.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Generation this handle was resolved under; valid even after the obituary retires the entry.
    pub(crate) fn generation(&self) -> u64 {
        self.pin.generation()
    }

    /// The cache pin a proxy `WIBinder` holds, as a `wp<BpBinder>` holds the `BpBinder`.
    pub(crate) fn pin(&self) -> &Arc<HandlePin> {
        &self.pin
    }

    /// Get the interface descriptor for this proxy.
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// `Weak` only for a proxy with this proxy's own handle; see module doc "Extension cache".
    fn classify_extension(&self, sib: &SIBinder) -> CachedExtension {
        if let Some(proxy) = (**sib).as_proxy() {
            if proxy.handle() == self.handle {
                return CachedExtension::Weak(SIBinder::downgrade(sib));
            }
        }
        CachedExtension::Strong(sib.clone())
    }

    /// Submit a transaction to the remote service.
    ///
    /// If a two-way call fails with a driver errno after the kernel took the
    /// transaction, its `BR_REPLY` still arrives, at this thread's next wait,
    /// and the thread's next two-way call returns it as its own reply, as in
    /// AOSP. The `thread_state` module doc ("Driver errors") has the cases.
    pub fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        // Fast-fail after obituary, as C++ `BpBinder::transact`; pairs with `send_obituary`.
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
}

// Delegates to the inherent methods, which generated `Bp*` code also calls via `as_proxy()`.
impl RemoteProxy for ProxyHandle {
    fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        ProxyHandle::prepare_transact(self, write_header)
    }
    fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
        ProxyHandle::submit_transact(self, code, data, flags)
    }
}

impl ProxyHandle {
    pub(crate) fn send_obituary(&self, who: &WIBinder) -> Result<()> {
        // Mirrors C++ `BpBinder::sendObituary`; see module doc "Death notifications".
        let recipients_snapshot: Vec<sync::Weak<dyn DeathRecipient>> = {
            let mut recipients = self.recipients.write().expect("Recipients lock poisoned");

            // Checked and set under the lock (as C++), so `Relaxed` suffices.
            if self.obituary_sent.load(Ordering::Relaxed) {
                return Ok(());
            }

            if !recipients.is_empty() {
                // Best effort, as AOSP: a failure must not stop the once-only obituary.
                if let Err(e) = thread_state::clear_death_notification(self.handle()) {
                    log::error!(
                        "clear_death_notification failed for handle {}: {e:?}; \
                         delivering the obituary anyway",
                        self.handle()
                    );
                }
            }

            let snapshot = std::mem::take(&mut *recipients);

            // `Release` for the lock-free `submit_transact` / `dump` Acquire loads.
            self.obituary_sent.store(true, Ordering::Release);

            snapshot
        };

        // Callbacks before the flush, so an ioctl failure cannot swallow the obituary.
        self.dispatch_obituary_callbacks(&recipients_snapshot, who);

        // On failure the command stays queued for `finish_obituary`'s phase-2 flush.
        if !recipients_snapshot.is_empty() {
            thread_state::flush_commands()?;
        }

        Ok(())
    }

    /// `binder_died` on each live recipient, panics caught; see module doc "Recipient panics".
    fn dispatch_obituary_callbacks(
        &self,
        snapshot: &[sync::Weak<dyn DeathRecipient>],
        who: &WIBinder,
    ) {
        for weak in snapshot {
            // Dead `Weak`s drop with the snapshot; `mem::take` already cleared the source.
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

    /// Send `DUMP_TRANSACTION` to the remote binder, which writes its
    /// state into `fd` — the transport under Android's `dumpsys <service>`,
    /// and under `rsb_service dump <name>` on Linux.
    ///
    /// `fd` is **consumed**: the parcel takes ownership of the descriptor
    /// and closes it once the transaction is done, so pass a duplicate when
    /// the caller needs to keep writing to the same file. (A deliberate
    /// divergence from AOSP `BpBinder::dump`, which calls
    /// `writeFileDescriptor(fd)` with `takeOwnership = false` and leaves
    /// the caller's fd open — the `Into<OwnedFd>` bound here makes the
    /// transfer explicit instead: a `File`, an `OwnedFd`, a `ChildStdin`
    /// and the other owning std types pass their fd, which closes exactly
    /// once whichever way the call ends.) The transaction is sent with
    /// `FLAG_CLEAR_BUF` where AOSP passes `0`: the kernel then zeroes the
    /// transaction buffer after the callee is done, harmless to the peer
    /// and cheap insurance for a dump that may carry sensitive state. The remote's handler is
    /// [`crate::Remotable::on_dump`], which the AIDL backend routes to
    /// [`crate::Interface::dump`]; the default implementation writes
    /// nothing and succeeds.
    ///
    /// `args` reach the handler verbatim; their meaning is the service's
    /// own. The call is synchronous, so it returns only after the remote
    /// has finished writing.
    pub fn dump<F: Into<OwnedFd>>(&self, fd: F, args: &[String]) -> Result<()> {
        // A dead proxy drops `fd` here, which closes it.
        if self.obituary_sent.load(Ordering::Acquire) {
            return Err(StatusCode::DeadObject);
        }
        let mut send = Parcel::new();
        // The parcel owns the fd from here and closes it on drop; a refused write closes it now.
        send.write_kernel_fd(fd.into())?;

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
        f.debug_struct("ProxyHandle")
            .field("handle", &self.handle)
            .field("descriptor", &self.descriptor)
            .field("stability", &self.stability)
            .field("obituary_sent", &self.obituary_sent)
            .finish()
    }
}

/// Identity is `(handle, generation)`, as the module doc "Proxy identity"
/// documents: the kernel recycles a handle number once its `binder_ref`
/// slot is released, so two `ProxyHandle`s with the same handle can name
/// different nodes. Matches `WIBinder`'s identity model.
impl PartialEq for ProxyHandle {
    fn eq(&self, other: &Self) -> bool {
        self.handle() == other.handle() && self.generation() == other.generation()
    }
}

impl Eq for ProxyHandle {}

impl Drop for ProxyHandle {
    /// AOSP `onLastStrongRef`; the `pin` field drops after this, as `~BpBinder` follows it.
    fn drop(&mut self) {
        // Unlinks before `BC_RELEASE`, as `kDecStrongLast` builds do (`BpBinder.cpp:880`, `:907`).
        let linked = !self
            .recipients
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_empty();
        if linked {
            // A failed clear stays counted, so the pin outlives the subscription anyway.
            if let Err(err) = thread_state::clear_death_notification(self.handle) {
                log::error!(
                    "BC_CLEAR_DEATH_NOTIFICATION for handle {} failed during Drop: {err:?}",
                    self.handle
                );
            }
        }
        // Safe: the cache pin's BC_INCREFS keeps the slot alive (module doc "Reference counts").
        if let Err(err) = thread_state::dec_strong_handle(self.handle) {
            log::error!(
                "BC_RELEASE for handle {} failed during Drop: {err:?}",
                self.handle
            );
        }
        // Only a proxy that reached `on_proxy_create` posts the drop (no phantom drops).
        if self.count_acquired {
            crate::proxy_count::on_proxy_drop(self.tracked_uid, self.counted_by_uid);
        }
    }
}

impl IBinder for ProxyHandle {
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        // 1. Cached answer; a weak (self-cycle) entry upgrades while the parent is held.
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
            Ok(Some(mut reply)) => DeserializeOption::deserialize_option(&mut reply)?,
            Ok(None) => None,
            // `UnknownTransaction` (pre-extension server) = none; others propagate, as AOSP.
            Err(StatusCode::UnknownTransaction) => None,
            Err(e) => return Err(e),
        };

        // 3. Cache: strong unless the handle aliases this proxy's own (see `ExtensionCache`).
        let entry = ext.as_ref().map(|sib| self.classify_extension(sib));
        let mut cache = self.extension.write().expect("Extension lock poisoned");
        *cache = ExtensionCache::Queried(entry);
        Ok(ext)
    }

    fn set_extension(&self, _extension: &SIBinder) -> Result<()> {
        // Server-side operation: a proxy cannot inform the remote; same as the default impl.
        Err(StatusCode::InvalidOperation)
    }

    /// Register a death notification for this object.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        // Lock before checking `obituary_sent`; see module doc "Death notifications".
        let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        // A dead weak would never fire: reject it before it takes a kernel subscription.
        if recipient.upgrade().is_none() {
            return Err(StatusCode::BadValue);
        }
        if recipients.is_empty() {
            // As C++ `BpBinder::linkToDeath`: write errors propagate, flush errors are ignored.
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
        // Lock before checking `obituary_sent`, as C++ `BpBinder::unlinkToDeath`.
        let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
        if self.obituary_sent.load(Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        }
        // First match only (C++ `removeAt(i)`); BC_CLEAR only on the non-empty→empty edge.
        let Some(i) = recipients
            .iter()
            .position(|r| sync::Weak::ptr_eq(r, &recipient))
        else {
            return Err(StatusCode::NameNotFound);
        };
        recipients.remove(i);
        if recipients.is_empty() {
            // Symmetric with `link_to_death`: write errors propagate, flush errors are ignored.
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

    // Ref-count methods are no-ops for a proxy; see module doc "Reference counts".

    fn inc_strong(&self, _strong: &SIBinder) -> Result<()> {
        Ok(())
    }

    fn attempt_inc_strong(&self) -> bool {
        // Meaningless for a proxy yet reachable via the public `IBinder`: warn, honor "succeed".
        log::warn!(
            "attempt_inc_strong called on a ProxyHandle (handle {}); \
             it is a no-op for proxies — use Weak<I>::upgrade",
            self.handle()
        );
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
    //! Most tests build proxies with `synthetic_proxy` (no `BC_ACQUIRE`, never in the proxy
    //! cache) and run without `ProcessState`: the fast-fail and position checks they cover
    //! return before any IPC.
    //!
    //! # Mutation gates
    //!
    //! - `proxy_downgrade_keeps_its_identity_without_a_cache_entry`: `synthetic_proxy` has no
    //!   cache entry, which is the state a death recipient's `downgrade` sees after the
    //!   obituary retired the entry. A `downgrade` that falls back to the `Native` variant
    //!   compares unequal to the obituary's `Proxy` weak; a second allocation with the same
    //!   `(handle, generation)` must still compare equal.
    //! - `test_unlink_to_death_removes_only_one_match`: the recipients vector is populated
    //!   directly so `link_to_death`'s `request_death_notification` IPC is not needed. An
    //!   unlink that removes every match (`Vec::retain`) drops the second registration.
    //! - `test_unlink_to_death_unregistered_returns_name_not_found`: an unlink of a never
    //!   registered recipient leaves the other registration intact. The "remove all matches"
    //!   mutant passes here and is caught only by `test_unlink_to_death_removes_only_one_match`;
    //!   this test covers the lookup path on its own.
    //! - `test_get_extension_strong_cache_does_not_auto_invalidate_on_dead_extension`: locks
    //!   in the non-invalidating cache described in the module doc "Extension cache"; an
    //!   auto-invalidate change flips this assertion on purpose.
    //! - `test_dump_fast_fails_and_closes_fd_when_obituary_sent`: `dump` takes a pipe's write
    //!   end and the test reads the read end. A fast-fail path that keeps the fd from dropping
    //!   (a `mem::forget`, a detach into a raw fd) leaves the read `EAGAIN` instead of EOF.
    //! - `test_dispatch_obituary_callbacks_isolates_panic`: the panicking recipient is first in
    //!   the two-element snapshot. Without the `catch_unwind` guard the panic unwinds past the
    //!   loop and the counting recipient is never called. The default hook prints the panic to
    //!   stderr; cargo buffers it per test.
    //! - `test_extension_cache_variant_holds_dual_modes`: `ExtensionCache::Queried` must admit
    //!   both variants. A strong-only cache re-creates the self-referencing `Arc<ProxyHandle>`
    //!   cycle; a weak-only cache re-creates the `BC_RELEASE` / `BC_ACQUIRE` churn that ends
    //!   in `BR_FAILED_REPLY` ("cannot find target node") under stress.

    use super::*;

    /// `ProxyHandle` with no kernel refs; `mem::forget` it to skip `BC_RELEASE` and `BC_DECREFS`.
    fn synthetic_proxy(obituary_sent: bool) -> Arc<ProxyHandle> {
        Arc::new(ProxyHandle {
            handle: 1,
            pin: HandlePin::synthetic(1, 1),
            descriptor: "test".to_string(),
            tracked_uid: 0,
            // `Drop` skips `on_proxy_drop`, as this skips `on_proxy_create`.
            count_acquired: false,
            counted_by_uid: false,
            stability: Stability::Local,
            obituary_sent: AtomicBool::new(obituary_sent),
            recipients: RwLock::new(Vec::new()),
            extension: RwLock::new(ExtensionCache::NotQueried),
        })
    }

    /// No-op recipient for building a `Weak<dyn DeathRecipient>`.
    struct NoopRecipient;
    impl DeathRecipient for NoopRecipient {
        fn binder_died(&self, _who: &WIBinder) {}
    }

    /// Keep the returned `Arc` alive: once it drops, `link_to_death` treats the `Weak` as dead.
    fn live_recipient_pair() -> (Arc<dyn DeathRecipient>, sync::Weak<dyn DeathRecipient>) {
        let arc: Arc<dyn DeathRecipient> = Arc::new(NoopRecipient);
        let weak = Arc::downgrade(&arc);
        (arc, weak)
    }

    /// A proxy's weak identity comes from the proxy, not the cache; see `# Mutation gates`.
    #[test]
    fn proxy_downgrade_keeps_its_identity_without_a_cache_entry() {
        let proxy = synthetic_proxy(false);
        let strong = SIBinder::from_arc(proxy.clone() as Arc<dyn IBinder>);

        let weak = SIBinder::downgrade(&strong);
        assert_eq!(weak, SIBinder::downgrade(&strong));

        // Distinct allocation, same `(handle, generation)`: equal only under proxy identity.
        let resurrected = synthetic_proxy(false);
        assert!(
            !Arc::ptr_eq(&proxy, &resurrected),
            "precondition: distinct allocations"
        );
        let resurrected_strong = SIBinder::from_arc(resurrected.clone() as Arc<dyn IBinder>);
        assert_eq!(
            weak,
            SIBinder::downgrade(&resurrected_strong),
            "same (handle, generation) is the same binder, whatever the allocation"
        );

        std::mem::forget(strong);
        std::mem::forget(resurrected_strong);
        std::mem::forget(proxy);
        std::mem::forget(resurrected);
    }

    /// The comparison a `DeathRecipient` actually writes.
    #[test]
    fn weak_compares_against_the_strong_it_came_from() {
        let proxy = synthetic_proxy(false);
        let strong = SIBinder::from_arc(proxy.clone() as Arc<dyn IBinder>);
        let who = SIBinder::downgrade(&strong);

        assert!(who == strong, "who == stored binder");
        assert!(strong == who, "and the operands commute");

        // A different binder must not match; a native one differs by variant already.
        struct Other;
        impl crate::Interface for Other {}
        impl crate::Remotable for Other {
            fn descriptor() -> &'static str {
                "rsbinder.test.proxy.IOther"
            }
            fn on_transact(
                &self,
                _code: crate::TransactionCode,
                _reader: &mut crate::Parcel,
                _reply: &mut crate::Parcel,
            ) -> Result<()> {
                Err(StatusCode::UnknownTransaction)
            }
            fn on_dump(&self, _w: &mut dyn std::io::Write, _args: &[String]) -> Result<()> {
                Ok(())
            }
        }
        let native = crate::Interface::as_binder(&crate::Binder::new(Other));
        assert!(who != native);

        std::mem::forget(strong);
        std::mem::forget(proxy);
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
            "ProxyHandle { handle: 1, descriptor: \"test\", stability: Local, obituary_sent: false }"
        );

        std::mem::forget(handle);
    }

    /// `DeadObject` without IPC, as C++ `BpBinder::transact`'s `if (mAlive)` (BpBinder.cpp:337).
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

    /// Rejected under the recipients lock before any IPC, as `BpBinder::linkToDeath` (:420).
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

    /// Matches C++ `BpBinder::unlinkToDeath` (BpBinder.cpp:456).
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

    /// Twice registered, once unlinked leaves one entry, as C++ `mObituaries->removeAt(i)`.
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

    /// No `BC_CLEAR_DEATH_NOTIFICATION` is queued: there is no subscription to clear.
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

    /// A dead extension stays cached and fast-fails with `DeadObject`; see `# Mutation gates`.
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

        // A cache hit returns the cached (dead) extension without a remote query.
        let returned = parent
            .get_extension()
            .expect("get_extension")
            .expect("cache hit should return Some");

        // It is the same dead ProxyHandle, so IPC through it must fast-fail.
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

        // Staleness is the contract: auto-invalidation would change the variant and trip this.
        let cache = parent.extension.read().expect("Extension lock poisoned");
        assert!(
            matches!(
                *cache,
                ExtensionCache::Queried(Some(CachedExtension::Strong(_)))
            ),
            "cache must remain Strong after get_extension on a dead extension"
        );
        drop(cache);

        // Drop `returned` first; forget `parent` so the synthetic `BC_RELEASE` Drop never runs.
        drop(returned);
        std::mem::forget(parent);
        std::mem::forget(ext_proxy);
    }

    /// `BadValue` before `BC_REQUEST_DEATH_NOTIFICATION`, leaving the recipients unchanged.
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

    /// A proxy cannot tell the remote, so it refuses; the cache stays `NotQueried`.
    #[test]
    fn test_set_extension_on_proxy_rejects_with_invalid_operation() {
        let proxy = synthetic_proxy(false);
        let ext = SIBinder::new(Arc::new(MockBinder)).expect("SIBinder::new");

        let result = proxy.set_extension(&ext);
        assert!(
            matches!(result, Err(StatusCode::InvalidOperation)),
            "expected InvalidOperation, got {result:?}"
        );

        // The cache must stay NotQueried, where the synthetic proxy started.
        let cache = proxy.extension.read().expect("Extension lock poisoned");
        assert!(
            matches!(*cache, ExtensionCache::NotQueried),
            "extension cache must remain NotQueried after a rejected set_extension"
        );
        drop(cache);
        std::mem::forget(proxy);
    }

    /// A dead proxy refuses the dump and closes the fd it was given; see `# Mutation gates`.
    #[test]
    fn test_dump_fast_fails_and_closes_fd_when_obituary_sent() {
        let (r, w) = crate::ParcelFileDescriptor::pipe().expect("pipe");
        let r: OwnedFd = r.into();
        rustix::fs::fcntl_setfl(&r, rustix::fs::OFlags::NONBLOCK).expect("non-blocking");

        let proxy = synthetic_proxy(true); // obituary_sent
        let result = proxy.dump(w, &[]);

        assert!(
            matches!(result, Err(StatusCode::DeadObject)),
            "expected DeadObject fast-fail, got {result:?}"
        );
        // EOF, not `EAGAIN`: no copy of the write end is left open.
        assert_eq!(
            rustix::io::read(&r, &mut [0u8; 1]),
            Ok(0),
            "the fast-fail path must close the fd `dump` took"
        );

        std::mem::forget(proxy);
    }

    /// Another recipient's registration survives; see `# Mutation gates`.
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

    /// Native `IBinder` for a `who` argument; its `downgrade` needs no `ProcessState`.
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

    /// A panicking recipient does not starve the next one; see `# Mutation gates`.
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

    /// `ExtensionCache::Queried` holds both variants; see `# Mutation gates`.
    #[test]
    fn test_extension_cache_variant_holds_dual_modes() {
        // Compile-time check: payload is `Option<CachedExtension>`; `_exhaust` pins the variants.
        let none_cache = ExtensionCache::Queried(None);
        let ExtensionCache::Queried(payload) = &none_cache else {
            unreachable!("constructed Queried, must match Queried")
        };
        let _typed: &Option<CachedExtension> = payload;

        // Exhaustiveness gate: fails to compile if a variant is removed or a third added.
        fn _exhaust(entry: &CachedExtension) -> &'static str {
            match entry {
                CachedExtension::Strong(_) => "strong",
                CachedExtension::Weak(_) => "weak",
            }
        }
    }
}

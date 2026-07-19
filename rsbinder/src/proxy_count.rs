// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Process-global proxy count + watermark + per-uid tracking.
//!
//! Mirrors AOSP `BpBinder::sBinderProxyCount` infrastructure
//! ([`BpBinder.cpp:78`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/BpBinder.cpp;l=78)
//! `sBinderProxyCount` global atomic; [`:914-955`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/BpBinder.cpp;l=914)
//! `getBinderProxyCount` / `setBinderProxyCountWatermarks` /
//! `setBinderProxyCountEventCallback` / `enableCountByUid`).
//!
//! The fast path is a single `Relaxed` atomic increment/decrement; per-uid
//! tracking is opt-in (`enable_count_by_uid`) and pays a `Mutex` lock per
//! proxy create/drop. Watermark callbacks fire **outside** the mutex to
//! avoid reentrant deadlock if the callback re-enters `proxy_count` APIs —
//! same discipline as AOSP's `postTask` defer-callback pattern.
//!
//! Defaults match AOSP: high=2500, low=2000, warning=2250. The watermark
//! state is process-global and shared between callbacks; `Limit`/`Warning`
//! are debounced (a uid only fires `Limit` once until it drops below `low`,
//! and `Warning` once until it drops below `warning`).

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

/// AOSP default high watermark ([`BpBinder.cpp:71`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/BpBinder.cpp;l=71)).
pub const DEFAULT_HIGH_WATERMARK: u64 = 2500;
/// AOSP default low watermark ([`BpBinder.cpp:73`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/BpBinder.cpp;l=73)).
pub const DEFAULT_LOW_WATERMARK: u64 = 2000;
/// AOSP default warning watermark ([`BpBinder.cpp:76`](https://cs.android.com/android/platform/superproject/+/android-16.0.0_r4:frameworks/native/libs/binder/BpBinder.cpp;l=76)).
pub const DEFAULT_WARNING_WATERMARK: u64 = 2250;

/// Event published by the proxy-count infrastructure when a per-uid
/// counter crosses a watermark.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyCountEvent {
    /// Per-uid count reached the warning watermark (between `warning`
    /// and `high`). Fired at most once per uid until the count drops
    /// below `warning`. AOSP `sWarningCallback`.
    Warning { uid: u32, count: u64 },
    /// Per-uid count reached the high watermark. Fired at most once per
    /// uid until the count drops below `low`. AOSP `sLimitCallback`.
    Limit { uid: u32, count: u64 },
}

/// Callback type for [`ProxyCountEvent`] notifications. Invoked **outside**
/// the internal `proxy_count` mutex, and — on the proxy-create path — after
/// the process-wide `ProcessState::handle_to_proxy` cache lock is released
/// (via [`CallbackDeferGuard`]). The callback may therefore freely re-enter
/// both `proxy_count` APIs and the binder proxy cache (`get_service`,
/// resolving/creating proxies). It must not block indefinitely — every proxy
/// create/drop on the firing thread is gated on it returning.
pub type ProxyCountCallback = Arc<dyn Fn(ProxyCountEvent) + Send + Sync>;

/// Process-global proxy count. Lock-free hot path: every
/// [`ProxyHandle`](crate::proxy::ProxyHandle) constructor `fetch_add`s
/// this and its `Drop` `fetch_sub`s. `Relaxed` ordering is sufficient
/// because the count is a monotonic-ish statistic, not a synchronization
/// primitive — readers see *some* recent value, not necessarily the
/// per-thread latest.
static PROXY_COUNT: AtomicU64 = AtomicU64::new(0);

/// Opt-in flag for per-uid tracking. Default `false` — the AOSP fast
/// path is "tracking off, lock not touched, only the global counter
/// moves." Setting this `true` does **not** retroactively populate the
/// uid map; only proxies created after the flip are tracked.
static COUNT_BY_UID_ENABLED: AtomicBool = AtomicBool::new(false);

struct State {
    high: u64,
    low: u64,
    warning: u64,
    callback: Option<ProxyCountCallback>,
    per_uid: HashMap<u32, UidEntry>,
}

#[derive(Default)]
struct UidEntry {
    count: u64,
    /// `Warning` already fired since the last time `count` dropped
    /// below `warning`. Debounces repeat callbacks.
    warning_fired: bool,
    /// `Limit` already fired since the last time `count` dropped
    /// below `low`. Debounces.
    limit_fired: bool,
}

static STATE: LazyLock<Mutex<State>> = LazyLock::new(|| {
    Mutex::new(State {
        high: DEFAULT_HIGH_WATERMARK,
        low: DEFAULT_LOW_WATERMARK,
        warning: DEFAULT_WARNING_WATERMARK,
        callback: None,
        per_uid: HashMap::new(),
    })
});

thread_local! {
    /// While set, `on_proxy_create` queues watermark callbacks instead of
    /// firing them inline. Set by [`CallbackDeferGuard`] around the
    /// proxy-cache create path so the callback never runs with the caller's
    /// `ProcessState::handle_to_proxy` write lock still held.
    static CALLBACK_DEFER: Cell<bool> = const { Cell::new(false) };
    /// Watermark callbacks queued while [`CALLBACK_DEFER`] was set, fired by
    /// the outermost [`CallbackDeferGuard`] on drop (after the cache lock is
    /// released).
    static PENDING_CALLBACKS: RefCell<Vec<(ProxyCountCallback, ProxyCountEvent)>> =
        const { RefCell::new(Vec::new()) };
}

/// RAII guard that defers watermark callbacks on the current thread until it
/// drops. `ProcessState::slow_path_p3` creates it **before** taking the
/// `handle_to_proxy` write lock and drops it **after** that lock is released
/// (declaration order), so a user callback that re-enters the proxy cache
/// (`get_service`, creating a proxy, …) cannot deadlock against the lock the
/// creating thread still holds. Nested guards keep deferring until the
/// outermost one drops. AOSP defers via `postTask`; this is the equivalent.
pub(crate) struct CallbackDeferGuard {
    was_deferring: bool,
}

impl CallbackDeferGuard {
    pub(crate) fn new() -> Self {
        let was_deferring = CALLBACK_DEFER.with(|d| d.replace(true));
        Self { was_deferring }
    }
}

impl Drop for CallbackDeferGuard {
    fn drop(&mut self) {
        CALLBACK_DEFER.with(|d| d.set(self.was_deferring));
        if self.was_deferring {
            // A nested guard is still active — keep deferring.
            return;
        }
        // Outermost guard: fire everything queued while deferral was active.
        // `CALLBACK_DEFER` is already cleared, so a callback that creates a
        // proxy fires its own watermark inline (the cache lock is released).
        let pending: Vec<_> = PENDING_CALLBACKS.with(|p| std::mem::take(&mut *p.borrow_mut()));
        for (cb, event) in pending {
            cb(event);
        }
    }
}

/// Snapshot of the process-global proxy count.
///
/// Includes every live [`ProxyHandle`](crate::proxy::ProxyHandle) —
/// kernel and (with `rpc` feature) RPC are NOT counted here unless they
/// also route through `ProxyHandle`, matching the AOSP "BpBinder kernel
/// proxies only" surface.
pub fn get_binder_proxy_count() -> u64 {
    PROXY_COUNT.load(Ordering::Relaxed)
}

/// Snapshot of the per-uid proxy count. Returns `0` if per-uid tracking
/// is disabled or this uid has never had a proxy created against it.
/// AOSP `BpBinder::getBinderProxyCount(uid)`.
pub fn get_binder_proxy_count_for_uid(uid: u32) -> u64 {
    let state = STATE.lock().expect("proxy_count state poisoned");
    state.per_uid.get(&uid).map(|e| e.count).unwrap_or(0)
}

/// Snapshot of every tracked uid's count, as `(uid, count)` pairs. The
/// returned `Vec` is a copy — callers may sort or filter without
/// holding the internal lock. AOSP `BpBinder::getCountByUid`.
pub fn get_binder_proxy_counts_by_uid() -> Vec<(u32, u64)> {
    let state = STATE.lock().expect("proxy_count state poisoned");
    state
        .per_uid
        .iter()
        .map(|(uid, e)| (*uid, e.count))
        .collect()
}

/// Configure the warning / high / low watermarks. AOSP
/// `BpBinder::setBinderProxyCountWatermarks(high, low, warning)`.
///
/// `warning` must be in `[low, high]` for the debounce logic to produce
/// the AOSP-faithful three-zone state machine (below-warning → warning
/// → limit, and resetting at `low`). Out-of-order values are accepted
/// silently — the state machine still terminates, just with a different
/// fire schedule.
pub fn set_binder_proxy_count_watermarks(high: u64, low: u64, warning: u64) {
    let mut state = STATE.lock().expect("proxy_count state poisoned");
    state.high = high;
    state.low = low;
    state.warning = warning;
}

/// Install (or clear, by passing `None`) the watermark event callback.
/// Replaces any prior callback. AOSP
/// `BpBinder::setBinderProxyCountEventCallback`.
pub fn set_binder_proxy_count_event_callback(callback: Option<ProxyCountCallback>) {
    let mut state = STATE.lock().expect("proxy_count state poisoned");
    state.callback = callback;
}

/// Enable or disable per-uid proxy tracking. When `enabled` is `false`,
/// the hot path skips the internal mutex entirely (matches AOSP's
/// `sCountByUidEnabled.load()` short-circuit). Disabling while uids are
/// tracked does **not** clear the existing map — re-enabling resumes
/// counting from where it left off; callers can `clear_count_by_uid`
/// for a hard reset.
pub fn enable_count_by_uid(enabled: bool) {
    COUNT_BY_UID_ENABLED.store(enabled, Ordering::Relaxed);
}

/// Whether per-uid tracking is currently enabled.
pub fn is_count_by_uid_enabled() -> bool {
    COUNT_BY_UID_ENABLED.load(Ordering::Relaxed)
}

/// Hard-reset the per-uid tracking map. Does not affect the global
/// counter or the enabled flag.
pub fn clear_count_by_uid() {
    let mut state = STATE.lock().expect("proxy_count state poisoned");
    state.per_uid.clear();
}

/// Internal hook called once per `ProxyHandle::new_acquired`. Bumps
/// the global counter (lock-free) and, if per-uid tracking is enabled,
/// updates the uid map under the state mutex.
///
/// Returns `true` iff the per-uid map was incremented (tracking was
/// enabled at create time). The caller records this on the proxy so its
/// `Drop` decrements the per-uid map iff this create did — binding the
/// decision per-object (AOSP `mTrackedUid`) rather than re-reading the
/// global flag, which would desync if tracking is toggled mid-flight.
///
/// Watermark callbacks fire after the mutex is dropped to support
/// reentrant callback bodies (AOSP `postTask` discipline).
pub(crate) fn on_proxy_create(uid: u32) -> bool {
    PROXY_COUNT.fetch_add(1, Ordering::Relaxed);
    if !COUNT_BY_UID_ENABLED.load(Ordering::Relaxed) {
        return false;
    }
    let event = {
        let mut state = STATE.lock().expect("proxy_count state poisoned");
        let high = state.high;
        let warning = state.warning;
        let entry = state.per_uid.entry(uid).or_default();
        entry.count += 1;
        let count = entry.count;
        let already_warning = entry.warning_fired;
        let already_limit = entry.limit_fired;
        if !already_limit && count >= high {
            entry.limit_fired = true;
            state
                .callback
                .as_ref()
                .map(|cb| (cb.clone(), ProxyCountEvent::Limit { uid, count }))
        } else if !already_warning && count >= warning && count < high {
            entry.warning_fired = true;
            state
                .callback
                .as_ref()
                .map(|cb| (cb.clone(), ProxyCountEvent::Warning { uid, count }))
        } else {
            None
        }
    };
    if let Some((cb, event)) = event {
        // Defer to after the proxy-cache lock is released when a
        // `CallbackDeferGuard` is active on this thread (the create path);
        // otherwise fire inline (already outside the internal `STATE` mutex).
        if CALLBACK_DEFER.with(|d| d.get()) {
            PENDING_CALLBACKS.with(|p| p.borrow_mut().push((cb, event)));
        } else {
            cb(event);
        }
    }
    true
}

/// Internal hook called from `ProxyHandle::drop`. Decrements the
/// global counter and, iff this proxy's `on_proxy_create` incremented the
/// per-uid map (`counted_by_uid`), decrements that entry and clears **both**
/// watermark debounce flags together once the count falls to/below `low`.
/// Symmetric with [`on_proxy_create`].
///
/// `counted_by_uid` is captured per-proxy at create time rather than
/// re-reading `COUNT_BY_UID_ENABLED` here: if tracking is toggled off while a
/// proxy is live, re-reading the live flag would skip the matching decrement
/// and permanently inflate the uid's count (and latch its watermark). AOSP
/// avoids this by deciding per-object via `BpBinder::mTrackedUid`.
///
/// AOSP (`BpBinder.cpp`) resets `LIMIT_REACHED_MASK | WARNING_REACHED_MASK`
/// jointly at `count <= low`. Clearing them at separate thresholds (warning
/// at `< warning`, limit at `< low`) let the `Warning` callback re-fire on a
/// `low <-> warning` oscillation, which AOSP never does — so we mirror the
/// joint reset.
pub(crate) fn on_proxy_drop(uid: u32, counted_by_uid: bool) {
    PROXY_COUNT.fetch_sub(1, Ordering::Relaxed);
    if !counted_by_uid {
        return;
    }
    let mut state = STATE.lock().expect("proxy_count state poisoned");
    let low = state.low;
    if let Some(entry) = state.per_uid.get_mut(&uid) {
        entry.count = entry.count.saturating_sub(1);
        if entry.count <= low {
            entry.limit_fired = false;
            entry.warning_fired = false;
        }
        if entry.count == 0 {
            state.per_uid.remove(&uid);
        }
    }
}

#[cfg(test)]
pub(crate) fn reset_for_test() {
    PROXY_COUNT.store(0, Ordering::Relaxed);
    COUNT_BY_UID_ENABLED.store(false, Ordering::Relaxed);
    CALLBACK_DEFER.with(|d| d.set(false));
    PENDING_CALLBACKS.with(|p| p.borrow_mut().clear());
    let mut state = STATE.lock().expect("proxy_count state poisoned");
    state.high = DEFAULT_HIGH_WATERMARK;
    state.low = DEFAULT_LOW_WATERMARK;
    state.warning = DEFAULT_WARNING_WATERMARK;
    state.callback = None;
    state.per_uid.clear();
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // `PROXY_COUNT` and the per-uid map are process-global. Every test here is
    // `#[serial_test::serial(binder)]` so it shares the `binder` serial group
    // with the proxy-creating `process_state` tests: on a real binder those
    // create `ProxyHandle`s that bump the global count in parallel, which would
    // otherwise pollute these exact-count assertions (a local mutex serialized
    // only the proxy_count tests against each other, not against the creators).

    #[test]
    #[serial_test::serial(binder)]
    fn global_counter_increments_and_decrements() {
        reset_for_test();
        assert_eq!(get_binder_proxy_count(), 0);
        on_proxy_create(1000);
        on_proxy_create(1000);
        on_proxy_create(2000);
        assert_eq!(get_binder_proxy_count(), 3);
        on_proxy_drop(1000, false);
        assert_eq!(get_binder_proxy_count(), 2);
        on_proxy_drop(1000, false);
        on_proxy_drop(2000, false);
        assert_eq!(get_binder_proxy_count(), 0);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn per_uid_disabled_by_default_skips_map() {
        reset_for_test();
        on_proxy_create(1000);
        on_proxy_create(2000);
        // Disabled → map stays empty even though global counter moved.
        assert_eq!(get_binder_proxy_count(), 2);
        assert_eq!(get_binder_proxy_count_for_uid(1000), 0);
        assert_eq!(get_binder_proxy_counts_by_uid(), vec![]);
        on_proxy_drop(1000, false);
        on_proxy_drop(2000, false);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn per_uid_enabled_tracks_each_uid() {
        reset_for_test();
        enable_count_by_uid(true);
        on_proxy_create(1000);
        on_proxy_create(1000);
        on_proxy_create(2000);
        assert_eq!(get_binder_proxy_count_for_uid(1000), 2);
        assert_eq!(get_binder_proxy_count_for_uid(2000), 1);
        assert_eq!(get_binder_proxy_count_for_uid(3000), 0);
        let mut snap = get_binder_proxy_counts_by_uid();
        snap.sort_by_key(|(uid, _)| *uid);
        assert_eq!(snap, vec![(1000, 2), (2000, 1)]);
        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
        on_proxy_drop(2000, true);
        // All counts at 0 → map entries removed.
        assert_eq!(get_binder_proxy_counts_by_uid(), vec![]);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn limit_callback_fires_once_until_low_watermark_resets() {
        reset_for_test();
        enable_count_by_uid(true);
        set_binder_proxy_count_watermarks(/*high*/ 5, /*low*/ 2, /*warning*/ 4);
        let fired = Arc::new(StdMutex::new(Vec::<ProxyCountEvent>::new()));
        let f = fired.clone();
        set_binder_proxy_count_event_callback(Some(Arc::new(move |event| {
            f.lock().unwrap().push(event);
        })));
        // Climb to high=5: warning fires at 4, limit at 5. Above 5 → no
        // re-fire while debounce sticks.
        for _ in 0..7 {
            on_proxy_create(1000);
        }
        let snap: Vec<_> = fired.lock().unwrap().clone();
        assert_eq!(snap.len(), 2, "warning + limit, once each: {snap:?}");
        assert!(matches!(
            snap[0],
            ProxyCountEvent::Warning { uid: 1000, .. }
        ));
        assert!(matches!(snap[1], ProxyCountEvent::Limit { uid: 1000, .. }));

        // Drop to below low=2 → debounce clears; next climb fires again.
        for _ in 0..6 {
            on_proxy_drop(1000, true);
        }
        assert_eq!(get_binder_proxy_count_for_uid(1000), 1);
        on_proxy_create(1000); // 2
        on_proxy_create(1000); // 3
        on_proxy_create(1000); // 4 → warning re-fires
        on_proxy_create(1000); // 5 → limit re-fires
        let snap: Vec<_> = fired.lock().unwrap().clone();
        assert_eq!(snap.len(), 4, "re-fire after below-low reset: {snap:?}");

        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn callback_runs_outside_state_lock() {
        reset_for_test();
        enable_count_by_uid(true);
        set_binder_proxy_count_watermarks(2, 1, 2);
        // Callback re-enters `get_binder_proxy_count_for_uid`, which
        // takes the state mutex. If the callback fired *under* the
        // outer lock this would deadlock; the test passing proves the
        // chokepoint deferred the callback after lock drop.
        set_binder_proxy_count_event_callback(Some(Arc::new(|_event| {
            let _snapshot = get_binder_proxy_count_for_uid(1000);
        })));
        on_proxy_create(1000);
        on_proxy_create(1000); // should not deadlock
        on_proxy_drop(1000, true);
        on_proxy_drop(1000, true);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn clear_count_by_uid_resets_map_only() {
        reset_for_test();
        enable_count_by_uid(true);
        on_proxy_create(1000);
        on_proxy_create(2000);
        assert_eq!(get_binder_proxy_count(), 2);
        clear_count_by_uid();
        assert_eq!(get_binder_proxy_counts_by_uid(), vec![]);
        // Global counter is unaffected — it tracks live `ProxyHandle`s,
        // not the per-uid statistic.
        assert_eq!(get_binder_proxy_count(), 2);
        on_proxy_drop(1000, true);
        on_proxy_drop(2000, true);
    }

    #[test]
    #[serial_test::serial(binder)]
    fn per_uid_count_survives_tracking_toggled_off_while_live() {
        // Regression: `on_proxy_drop` must mirror what the proxy's
        // `on_proxy_create` did (captured per-object), not re-read the live
        // `COUNT_BY_UID_ENABLED`. Toggling tracking off while a counted proxy
        // is alive previously skipped its decrement and permanently inflated
        // the uid's count.
        reset_for_test();

        enable_count_by_uid(true);
        // Two proxies created while enabled → each captures counted_by_uid=true.
        let c1 = on_proxy_create(1000);
        let c2 = on_proxy_create(1000);
        assert!(c1 && c2);
        assert_eq!(get_binder_proxy_count_for_uid(1000), 2);

        // Tracking turned off while both proxies are still live.
        enable_count_by_uid(false);

        // A proxy created *now* captures counted_by_uid=false.
        let c3 = on_proxy_create(1000);
        assert!(!c3);

        // Drop all three using each proxy's captured decision. The two
        // enabled-era proxies decrement the per-uid map even though tracking
        // is now off; the disabled-era proxy does not touch it.
        on_proxy_drop(1000, c3); // disabled-era: no per-uid effect
        on_proxy_drop(1000, c2);
        on_proxy_drop(1000, c1);

        // Count is back to exactly 0 (entry removed) — no inflation.
        assert_eq!(get_binder_proxy_count_for_uid(1000), 0);
        assert_eq!(get_binder_proxy_counts_by_uid(), vec![]);
    }
}

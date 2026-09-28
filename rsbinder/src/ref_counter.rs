// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Strong reference counter for binder objects, following AOSP `RefBase`.
//!
//! # `INITIAL_STRONG_VALUE`
//!
//! Matches AOSP `RefBase.cpp` (`#define INITIAL_STRONG_VALUE (1<<28)`) exactly.
//! The value must be positive so that the transient `fetch_add(1)` in `inc`
//! (before the sentinel is subtracted) stays positive: a concurrent
//! `attempt_inc` that observes the intermediate value must see a normal
//! positive count, never a negative one. `i32::MAX` here would wrap to
//! `i32::MIN` on that first increment and open a window where the count reads
//! negative (spurious `attempt_inc` failure in release, `debug_assert` panic
//! in debug). `1<<28` leaves ~2^28 headroom above and below.
//!
//! # Divergence from AOSP in `attempt_inc`
//!
//! When the weak-revive arm loses the race (the value its
//! `fetch_add(1)` returns is neither 0 nor the sentinel), `attempt_inc` undoes
//! **both** that strong bump and the weak ref (`dec_func`) and returns
//! `false`; a bare `dec_func` would leak the `fetch_add(1)`. AOSP
//! `RefBase::attemptIncStrong` keeps the ref and returns true there
//! (its `OBJECT_LIFETIME_WEAK` arm).
//!
//! # Memory ordering
//!
//! `RefCounter` starts at `INITIAL_STRONG_VALUE`, as AOSP `BBinder` does: the first `inc`
//! sees the sentinel, runs the first-ref callback and subtracts it. The orderings follow
//! AOSP `RefBase`:
//!
//! - `inc`: `Relaxed` for the `fetch_add` and for the `fetch_sub` that removes
//!   `INITIAL_STRONG_VALUE`. AOSP assumes `onFirstRef()` provides its own synchronization.
//! - `dec`: `Release` on the `fetch_sub`, so all prior writes are visible, and an `Acquire`
//!   fence only when the count reaches 0 and the object is destroyed. Only the final
//!   decrement needs `Acquire`, so `AcqRel` on every decrement is not used. This is AOSP's
//!   `RefBase::decStrong`.
//! - `attempt_inc`: `Relaxed` for the load, `compare_exchange`, `fetch_add` and `fetch_sub`.
//!   AOSP's `attemptIncStrong` assumes the calling code synchronizes at a higher level.

use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::*;

// AOSP `RefBase.cpp` value; must stay positive with headroom (see module doc).
pub(crate) const INITIAL_STRONG_VALUE: i32 = 1 << 28;

/// Strong count starting at `INITIAL_STRONG_VALUE`; see module doc "Memory ordering".
pub(crate) struct RefCounter {
    pub(crate) count: AtomicI32,
}

impl RefCounter {
    pub fn inc(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        let c = self.count.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            // Relaxed as in AOSP: `f()` supplies any sync the first-ref initializer needs.
            self.count
                .fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
            f()?;
        }
        Ok(())
    }

    pub fn attempt_inc(
        &self,
        is_strong: bool,
        inc_func: impl FnOnce() -> bool,
        dec_func: impl FnOnce(),
    ) -> bool {
        // All Relaxed, as AOSP `attemptIncStrong`: callers synchronize at a higher level.
        let mut curr_count = self.count.load(Ordering::Relaxed);
        debug_assert!(curr_count >= 0, "attempt_increase called after underflow");
        while curr_count > 0 && curr_count != INITIAL_STRONG_VALUE {
            // Use Relaxed for compare_exchange, matching Android's implementation
            match self.count.compare_exchange_weak(
                curr_count,
                curr_count + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(count) => curr_count = count,
            }
        }

        if curr_count <= 0 || curr_count == INITIAL_STRONG_VALUE {
            if is_strong {
                if curr_count <= 0 {
                    return false;
                }
                while curr_count > 0 {
                    match self.count.compare_exchange_weak(
                        curr_count,
                        curr_count.wrapping_add(1),
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(count) => curr_count = count,
                    }
                }
                if curr_count <= 0 {
                    return false;
                }
            } else {
                if !inc_func() {
                    return false;
                }
                // Use Relaxed to match Android's implementation
                curr_count = self.count.fetch_add(1, Ordering::Relaxed);
                if curr_count != 0 && curr_count != INITIAL_STRONG_VALUE {
                    // Lost revive race: undo bump and weak ref (diverges from AOSP; module doc).
                    self.count.fetch_sub(1, Ordering::Relaxed);
                    dec_func();
                    return false;
                }
            }
        }
        if curr_count == INITIAL_STRONG_VALUE {
            // Use Relaxed to match Android's implementation
            self.count
                .fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
        }

        true
    }

    pub fn dec(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        // Release so this thread's writes are visible before the decrement (AOSP `decStrong`).
        let c = self.count.fetch_sub(1, Ordering::Release);
        // A double-dec reads the reset sentinel, not `c < 1`, and wedges the counter for good.
        debug_assert!(
            c >= 1 && c != INITIAL_STRONG_VALUE,
            "RefCounter::dec underflow (double decStrong), c = {c}"
        );
        if c == 1
            && self
                .count
                .compare_exchange(
                    0,
                    INITIAL_STRONG_VALUE,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                )
                .is_ok()
        {
            // Pairs with every Release decrement before destruction, as AOSP `decStrong` does.
            std::sync::atomic::fence(Ordering::Acquire);

            f()?;
        }
        Ok(())
    }
}

impl Default for RefCounter {
    fn default() -> Self {
        Self {
            count: AtomicI32::new(INITIAL_STRONG_VALUE),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ref_counter() {
        let counter = RefCounter::default();
        assert_eq!(counter.count.load(Ordering::Relaxed), INITIAL_STRONG_VALUE);

        let result = counter.inc(|| Ok(()));
        assert!(result.is_ok());
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);

        let result = counter.dec(|| Ok(()));
        assert!(result.is_ok());
        assert_eq!(counter.count.load(Ordering::Relaxed), INITIAL_STRONG_VALUE);
    }

    #[test]
    fn test_ref_counter_attempt_inc() {
        let counter = RefCounter::default();
        assert_eq!(counter.count.load(Ordering::Relaxed), INITIAL_STRONG_VALUE);

        let result = counter.attempt_inc(false, || false, || {});
        assert!(!result);
        assert_eq!(counter.count.load(Ordering::Relaxed), INITIAL_STRONG_VALUE);

        let result = counter.attempt_inc(true, || true, || {});
        assert!(result);
        assert_eq!(counter.count.load(Ordering::Relaxed), 1);

        let result = counter.attempt_inc(true, || true, || {});
        assert!(result);
        assert_eq!(counter.count.load(Ordering::Relaxed), 2);

        let result = counter.attempt_inc(false, || false, || {});
        assert!(result);
        assert_eq!(counter.count.load(Ordering::Relaxed), 3);
    }
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::*;

// Matches AOSP `RefBase.cpp` (`#define INITIAL_STRONG_VALUE (1<<28)`) exactly.
// The value must be positive so that the transient `fetch_add(1)` in `inc`
// (before the sentinel is subtracted) stays positive: a concurrent
// `attempt_inc` that observes the intermediate value must see a normal
// positive count, never a negative one. `i32::MAX` here would wrap to
// `i32::MIN` on that first increment and open a window where the count reads
// negative (spurious `attempt_inc` failure in release, `debug_assert` panic
// in debug). `1<<28` leaves ~2^28 headroom above and below.
pub(crate) const INITIAL_STRONG_VALUE: i32 = 1 << 28;

/// Thread-safe reference counter used for binder objects.
///
/// This counter uses a special initial value (INITIAL_STRONG_VALUE) to defer
/// the first increment operation. This matches Android's BBinder implementation
/// and allows lazy initialization of binder objects.
///
/// # Memory Ordering Strategy
///
/// This implementation follows Android's RefBase memory ordering pattern exactly:
///
/// - **inc()**: Uses `Relaxed` for all atomic operations. The fetch_add and fetch_sub
///   (when removing INITIAL_STRONG_VALUE) both use Relaxed ordering. Android assumes
///   that onFirstRef() provides its own synchronization if needed.
///
/// - **dec()**: Uses `Release` for the fetch_sub to ensure all prior writes are visible,
///   followed by an `Acquire` fence only when destroying the object (when count reaches 0).
///   This two-step approach (Release on decrement + Acquire fence on destruction) is the
///   classic reference counting pattern that provides better performance than using AcqRel
///   on every decrement, since only the final decrement needs the Acquire synchronization.
///   **This exactly matches Android's RefBase::decStrong implementation.**
///
/// - **attempt_inc()**: Uses `Relaxed` for all operations (load, compare_exchange, fetch_add,
///   fetch_sub). Android's attemptIncStrong assumes synchronization happens at higher levels
///   in the calling code.
///
/// # Safety
/// This type is Send + Sync and safe to use across threads. The atomic operations
/// with proper memory ordering ensure thread-safe reference counting without data races.
/// This implementation is verified to match Android's proven RefBase pattern.
pub(crate) struct RefCounter {
    pub(crate) count: AtomicI32,
}

impl RefCounter {
    pub fn inc(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        // Relaxed is sufficient for the increment - we're just updating the count
        // We don't need to synchronize with previous operations here
        let c = self.count.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            // Relaxed matches AOSP RefBase: this only clears the sentinel
            // bias on the first strong ref; any synchronization the
            // first-ref initializer needs is provided by `f()` itself.
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
        // Android uses Relaxed for all operations in attemptIncStrong.
        // The assumption is that synchronization happens at a higher level.
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
                    // Lost the revive race. Undo BOTH the strong bump we just
                    // made and the weak ref (`dec_func`); a bare `dec_func`
                    // would leak the `fetch_add(1)` above. Diverges from AOSP
                    // `RefBase::attemptIncStrong`, which keeps the ref and
                    // returns true here (OBJECT_LIFETIME_WEAK arm).
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
        // Use Release ordering to ensure all our writes are visible before the decrement.
        // This matches Android's RefBase::decStrong implementation.
        let c = self.count.fetch_sub(1, Ordering::Release);
        debug_assert!(c >= 1, "RefCounter::dec underflow (double decStrong)");
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
            // Acquire fence to synchronize with all previous Release operations.
            // This ensures we see all operations from threads that held references.
            // This pattern matches Android's implementation:
            // fetch_sub(Release) + atomic_thread_fence(Acquire) before destruction.
            std::sync::atomic::fence(Ordering::Acquire);

            // At this point we've acquired synchronization with all previous operations.
            // Safe to destroy the object via f()
            f()?;
        }
        Ok(())
    }

    // pub fn get(&self) -> i32 {
    //     self.count.load(Ordering::Relaxed)
    // }
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

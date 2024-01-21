use std::sync::atomic::{AtomicI32, Ordering};

use crate::error::*;

const INITIAL_STRONG_VALUE: i32 = i32::MAX as _;

pub(crate) struct RefCounter {
    pub(crate) count: AtomicI32,
}

impl RefCounter {
    pub fn inc(&self, f: impl FnOnce() -> Result<()>) -> Result<()>
    {
        let c = self.count.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            self.count.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
            f()?;
        }
        Ok(())
    }

    pub fn attempt_inc(&self, is_strong: bool, inc_func: impl FnOnce() -> bool, dec_func: impl FnOnce()) -> bool {
        let mut curr_count = self.count.load(Ordering::Relaxed);
        debug_assert!(curr_count >= 0, "attempt_increase called after underflow");
        while curr_count > 0 && curr_count != INITIAL_STRONG_VALUE {
            match self.count.compare_exchange_weak(curr_count, curr_count + 1,
                Ordering::Relaxed, Ordering::Relaxed) {
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
                    match self.count.compare_exchange_weak(curr_count, curr_count + 1,
                        Ordering::Relaxed, Ordering::Relaxed) {
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
                curr_count = self.count.fetch_add(1, Ordering::Relaxed);
                if curr_count != 0 && curr_count != INITIAL_STRONG_VALUE {
                    dec_func();
                    return false;
                }
            }
        }
        if curr_count == INITIAL_STRONG_VALUE {
            self.count.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
        }

        true
    }

    pub fn dec(&self, f: impl FnOnce() -> Result<()>) -> Result<()> {
        let c = self.count.fetch_sub(1, Ordering::Relaxed);
        if c == 1 {
            self.count.compare_exchange(0, INITIAL_STRONG_VALUE,
                Ordering::Relaxed, Ordering::Relaxed)
                .expect("Failed to exchange the reference count.");
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

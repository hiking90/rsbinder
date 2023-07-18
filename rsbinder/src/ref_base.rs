use std::ops::Deref;
use std::sync::{Arc, atomic::*};
use crate::{
    IBinder,
    thread_state,
    error::*,
};

const INITIAL_STRONG_VALUE: usize = i32::MAX as _;

// #[derive(Debug)]
struct Inner {
    strong: AtomicUsize,
    data: Box<dyn IBinder>,
}

impl Inner {
    fn new(data: Box<dyn IBinder>) -> Arc<Self> {
        Arc::new(
            Self {
                strong: AtomicUsize::new(INITIAL_STRONG_VALUE),
                data,
            }
        )
    }
}

impl Drop for Inner {
    fn drop(self: &mut Inner) {
        if let Some(proxy) = self.data.as_proxy() {
            thread_state::dec_weak_handle(proxy.handle())
                .expect("Failed to decrease the binder weak reference count.")
        }
    }
}

// #[derive(Debug)]
pub struct Strong {
    inner: Arc<Inner>,
}

impl Strong {
    pub fn new(data: Box<dyn IBinder>) -> Self {
        let this = Weak::new(data).upgrade();
        this.inc_strong();
        this
    }

    fn new_with_inner(inner: Arc<Inner>) -> Self {
        let this = Self { inner };
        this.inc_strong();
        this
    }

    pub fn downgrade(this: &Self) -> Weak {
        Weak::new_with_inner(this.inner.clone())
        // drop will be called.
    }

    fn inc_strong(&self) {
        let c = self.inner.strong.fetch_add(1, Ordering::Relaxed);
        if c == INITIAL_STRONG_VALUE {
            self.inner.strong.fetch_sub(INITIAL_STRONG_VALUE, Ordering::Relaxed);
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::inc_strong_handle(proxy.handle(), self.clone())
                    .expect("Failed to increase the binder strong reference count.");
            }
        }
    }

    fn dec_strong(&self) {
        let c = self.inner.strong.fetch_sub(1, Ordering::Relaxed);
        if c == 1 {
            if let Some(proxy) = self.inner.data.as_proxy() {
                thread_state::dec_strong_handle(proxy.handle())
                    .expect("Failed to decrease the binder strong reference count.");
            }
        }
    }
}

impl Clone for Strong {
    fn clone(&self) -> Self {
        Self::new_with_inner(self.inner.clone())
    }
}

impl Drop for Strong {
    fn drop(&mut self) {
        self.dec_strong();
    }
}

impl Deref for Strong {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

#[derive(Clone)]
pub struct Weak {
    inner: Arc<Inner>,
}

impl Weak {
    pub(crate) fn new(data: Box<dyn IBinder>) -> Self {
        let this = Self { inner: Inner::new(data) };

        if let Some(proxy) = this.inner.data.as_proxy() {
            thread_state::inc_weak_handle(proxy.handle(), this)
                .expect("Failed to increase the binder weak reference count.")
        }

        this
    }

    fn new_with_inner(inner: Arc<Inner>) -> Self {
        Self { inner: inner }
    }

    pub fn upgrade(&self) -> Strong {
        Strong::new_with_inner(self.inner.clone())
    }
}

impl Deref for Weak {
    type Target = Box<dyn IBinder>;
    fn deref(&self) -> &Self::Target {
        &self.inner.data
    }
}

#[cfg(test)]
mod tests {
    use crate::proxy::Proxy;
    use super::*;

    #[test]
    fn test_strong() -> Result<()> {
        let strong = Strong::new(Proxy::new_unknown(0));
        assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 1);

        let strong2 = strong.clone();
        assert_eq!(strong2.inner.strong.load(Ordering::Relaxed), 2);


        let weak = Strong::downgrade(&strong);

        assert_eq!(weak.inner.strong.load(Ordering::Relaxed), 1);

        let strong = weak.upgrade();
        assert_eq!(strong.inner.strong.load(Ordering::Relaxed), 2);
        Strong::downgrade(&strong);
        // assert_eq!(*strong2.0.lock().unwrap(), 101);

        // let weak = strong2.downgrade();

        // assert_eq!(*weak.0.lock().unwrap(), 1);

        Ok(())
    }
}

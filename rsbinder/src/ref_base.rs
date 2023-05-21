use std::rc::Rc;
use std::borrow::Borrow;
use std::ops::Deref;
use std::fmt;
use std::sync::atomic::Ordering;
use std::marker::PhantomData;
use std::os::raw::c_void;
use std::sync::atomic;

use crate::binder::*;

const OBJECT_LIFETIME_STRONG: i32   = 0x0000;
const OBJECT_LIFETIME_WEAK: i32     = 0x0001;
const OBJECT_LIFETIME_MASK: i32     = 0x0001;
const INITIAL_STRONG_VALUE: i32     = 1<<28;
const FIRST_INC_STRONG: i32         = 0x0001;

// pub trait Referenceable {
//     fn on_first(&mut self);
//     fn on_last(&mut self);
//     fn on_last_weak(&mut self);
//     fn on_inc_strong_attempted(&mut self, _flags: u32);
// }

// pub(crate) struct RemoteRef;

// impl Referenceable for RemoteRef {
//     fn on_first(&mut self) {}
//     fn on_last(&mut self) {}
//     fn on_last_weak(&mut self) {}
//     fn on_inc_strong_attempted(&mut self, _flags: u32) {}
// }

#[repr(C)]
struct Inner<T> {
    strong: atomic::AtomicI32,
    weak: atomic::AtomicI32,
    ptr: *mut T,
    flags: atomic::AtomicI32,
}

impl<T>  Inner<T>  {
    pub(crate) fn new(ptr: *mut T) -> Self {
        Self {
            strong: atomic::AtomicI32::new(INITIAL_STRONG_VALUE),
            weak: atomic::AtomicI32::new(0),
            ptr: ptr,
            flags: atomic::AtomicI32::new(OBJECT_LIFETIME_STRONG),
        }
    }

    pub(crate) fn from_raw<'a>(raw: *mut c_void) -> &'a mut Self {
        unsafe { &mut *(raw as *mut Inner<T>) }
    }

    pub(crate) fn ptr(&self) -> *mut T {
        self.ptr
    }

    pub(crate) fn inc_weak(&mut self) {
        self.weak.fetch_add(1, atomic::Ordering::Relaxed);
    }

    // pub(crate) fn inc_weak_require_weak(&mut self) {
    // }

    pub(crate) fn dec_weak(&mut self) {
        let c = self.weak.fetch_sub(1, atomic::Ordering::Release);
        if c == 1 {
            atomic::fence(atomic::Ordering::Acquire);

            let flags = self.flags.load(atomic::Ordering::Relaxed);
            if (flags & OBJECT_LIFETIME_MASK) == OBJECT_LIFETIME_STRONG {
                // This is the regular lifetime case. The object is destroyed
                // when the last strong reference goes away. Since weakref_impl
                // outlives the object, it is not destroyed in the dtor, and
                // we'll have to do it here.
                if self.strong.load(atomic::Ordering::Relaxed) == INITIAL_STRONG_VALUE {
                    // Decrementing a weak count to zero when object never had a strong
                    // reference.  We assume it acquired a weak reference early, e.g.
                    // in the constructor, and will eventually be properly destroyed,
                    // usually via incrementing and decrementing the strong count.
                    // Thus we no longer do anything here.  We log this case, since it
                    // seems to be extremely rare, and should not normally occur. We
                    // used to deallocate mBase here, so this may now indicate a leak.
                    log::warn!("RefBase: Object at {:?} lost last weak reference before it had a strong reference",
                        self.ptr);
                } else {
                    drop(self)
                }
            } else {
                // This is the OBJECT_LIFETIME_WEAK case. The last weak-reference
                // is gone, we can destroy the object.
                unsafe {
                    // self.on_last_weak();
                    libc::free(self.ptr as _)
                }
            }
        }
    }

    pub(crate) fn attempt_inc_strong(&mut self) -> bool {
        let mut cur_count = self.strong.load(atomic::Ordering::Relaxed);

        while cur_count > 0 && cur_count != INITIAL_STRONG_VALUE {
            // we're in the easy/common case of promoting a weak-reference
            // from an existing strong reference.
            if self.strong.compare_exchange_weak(cur_count, cur_count + 1,
                atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
                break;
            }
            // the strong count has changed on us, we need to re-assert our
            // situation. curCount was updated by compare_exchange_weak.
        }

        if cur_count <= 0 || cur_count == INITIAL_STRONG_VALUE {
            // we're now in the harder case of either:
            // - there never was a strong reference on us
            // - or, all strong references have been released
            let flags = self.flags.load(atomic::Ordering::Relaxed);
            if (flags & OBJECT_LIFETIME_MASK) == OBJECT_LIFETIME_STRONG {
                // this object has a "normal" life-time, i.e.: it gets destroyed
                // when the last strong reference goes away
                if cur_count <= 0 {
                    return false;
                }

                // here, curCount == INITIAL_STRONG_VALUE, which means
                // there never was a strong-reference, so we can try to
                // promote this object; we need to do that atomically.
                while cur_count > 0 {
                    if self.strong.compare_exchange_weak(cur_count, cur_count + 1,
                        atomic::Ordering::Relaxed, atomic::Ordering::Relaxed).is_ok() {
                        break;
                    }
                    // the strong count has changed on us, we need to re-assert our
                    // situation (e.g.: another thread has inc/decStrong'ed us)
                    // curCount has been updated.
                }

                if cur_count <= 0 {
                    // promote() failed, some other thread destroyed us in the
                    // meantime (i.e.: strong count reached zero).
                    return false;
                }
            } else {
                // this object has an "extended" life-time, i.e.: it can be
                // revived from a weak-reference only.
                // Ask the object's implementation if it agrees to be revived

//             if (!impl->mBase->onIncStrongAttempted(FIRST_INC_STRONG, id)) {
//                 // it didn't so give-up.
//                 decWeak(id);
//                 return false;
//             }
                // grab a strong-reference, which is always safe due to the
                // extended life-time.
                cur_count = self.strong.fetch_add(1, atomic::Ordering::Relaxed);
            // If the strong reference count has already been incremented by
            // someone else, the implementor of onIncStrongAttempted() is holding
            // an unneeded reference.  So call onLastStrongRef() here to remove it.
            // (No, this is not pretty.)  Note that we MUST NOT do this if we
            // are in fact acquiring the first reference.
                if cur_count != 0 && cur_count != INITIAL_STRONG_VALUE {
                    // impl->mBase->onLastStrongRef(id);
                }
            }
        }

        // curCount is the value of mStrong before we incremented it.
        // Now we need to fix-up the count if it was INITIAL_STRONG_VALUE.
        // This must be done safely, i.e.: handle the case where several threads
        // were here in attemptIncStrong().
        // curCount > INITIAL_STRONG_VALUE is OK, and can happen if we're doing
        // this in the middle of another incStrong.  The subtraction is handled
        // by the thread that started with INITIAL_STRONG_VALUE.

        if cur_count == INITIAL_STRONG_VALUE {
            self.strong.fetch_sub(INITIAL_STRONG_VALUE, atomic::Ordering::Relaxed);
        }

        true
    }

    // fn on_first(&mut self) {
    //     let referenceable = unsafe { &mut *self.ptr};
    //     referenceable.on_first()
    // }

    // fn on_last(&mut self) {
    //     let referenceable = unsafe { &mut *self.ptr};
    //     referenceable.on_last()
    // }

    // fn on_last_weak(&mut self) {
    //     let referenceable = unsafe { &mut *self.ptr};
    //     referenceable.on_last_weak()
    // }

    // fn on_inc_strong_attempted(&mut self, flags: u32) {
    //     let referenceable = unsafe { &mut *self.ptr};
    //     referenceable.on_inc_strong_attempted(flags)
    // }
}


/// Strong reference to a binder object
pub struct Strong<I: FromIBinder + ?Sized>{
    inner: Rc<*mut Inner<I>>,
}

impl<I: FromIBinder + ?Sized> Strong<I> {
    /// Create a new strong reference to the provided binder object
    pub fn new(binder: Box<I>) -> Self {
        let binder_ptr = Box::<I>::into_raw(binder);
        Strong {
            inner: Rc::new(Box::into_raw(Box::new(Inner::<I>::new(binder_ptr))))
        }
    }

    /// Construct a new weak reference to this binder
    pub fn downgrade(this: &Strong<I>) -> Weak<I> {
        Weak::new(this)
    }

    // /// Convert this synchronous binder handle into an asynchronous one.
    // pub fn into_async<P>(self) -> Strong<<I as ToAsyncInterface<P>>::Target>
    // where
    //     I: ToAsyncInterface<P>,
    // {
    //     // By implementing the ToAsyncInterface trait, it is guaranteed that the binder
    //     // object is also valid for the target type.
    //     FromIBinder::try_from(self.0.as_binder()).unwrap()
    // }

    // /// Convert this asynchronous binder handle into a synchronous one.
    // pub fn into_sync(self) -> Strong<<I as ToSyncInterface>::Target>
    // where
    //     I: ToSyncInterface,
    // {
    //     // By implementing the ToSyncInterface trait, it is guaranteed that the binder
    //     // object is also valid for the target type.
    //     FromIBinder::try_from(self.0.as_binder()).unwrap()
    // }
}

impl<I: FromIBinder + ?Sized> Clone for Strong<I> {
    fn clone(&self) -> Self {
        // Since we hold a strong reference, we should always be able to create
        // a new strong reference to the same interface type, so try_from()
        // should never fail here.
        FromIBinder::try_from(self.0.as_binder()).unwrap()
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

impl<I: FromIBinder + fmt::Debug + ?Sized> fmt::Debug for Strong<I> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        fmt::Debug::fmt(&**self, f)
    }
}

impl<I: FromIBinder + ?Sized> Ord for Strong<I> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.as_binder().cmp(&other.0.as_binder())
    }
}

impl<I: FromIBinder + ?Sized> PartialOrd for Strong<I> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.0.as_binder().partial_cmp(&other.0.as_binder())
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
    // weak_binder: WpIBinder,
    interface_type: PhantomData<I>,
}

impl<I: FromIBinder + ?Sized> Weak<I> {
    /// Construct a new weak reference from a strong reference
    fn new(binder: &Strong<I>) -> Self {
        let weak_binder = binder.as_binder().downgrade();
        Weak {
            weak_binder,
            interface_type: PhantomData,
        }
    }

    // /// Upgrade this weak reference to a strong reference if the binder object
    // /// is still alive
    // pub fn upgrade(&self) -> Result<Strong<I>> {
    //     self.weak_binder
    //         .promote()
    //         .ok_or(StatusCode::DEAD_OBJECT)
    //         .and_then(FromIBinder::try_from)
    // }
}

impl<I: FromIBinder + ?Sized> Clone for Weak<I> {
    fn clone(&self) -> Self {
        Self {
            weak_binder: self.weak_binder.clone(),
            interface_type: PhantomData,
        }
    }
}

impl<I: FromIBinder + ?Sized> Ord for Weak<I> {
    fn cmp(&self, other: &Self) -> Ordering {
        self.weak_binder.cmp(&other.weak_binder)
    }
}

impl<I: FromIBinder + ?Sized> PartialOrd for Weak<I> {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        self.weak_binder.partial_cmp(&other.weak_binder)
    }
}

impl<I: FromIBinder + ?Sized> PartialEq for Weak<I> {
    fn eq(&self, other: &Self) -> bool {
        self.weak_binder == other.weak_binder
    }
}

impl<I: FromIBinder + ?Sized> Eq for Weak<I> {}


// pub(crate) struct RefBase<T: Referenceable>(ManuallyDrop<Box<Inner<T>>>);



        // // acquires a strong reference if there is already one.
        // bool                attemptIncStrong(const void* id);

        // // acquires a weak reference if there is already one.
        // // This is not always safe. see ProcessState.cpp and BpBinder.cpp
        // // for proper use.
        // bool                attemptIncWeak(const void* id);

        // //! DEBUGGING ONLY: Get current weak ref count.
        // int32_t             getWeakCount() const;

        // //! DEBUGGING ONLY: Print references held on object.
        // void                printRefs() const;

        // //! DEBUGGING ONLY: Enable tracking for this object.
        // // enable -- enable/disable tracking
        // // retain -- when tracking is enable, if true, then we save a stack trace
        // //           for each reference and dereference; when retain == false, we
        // //           match up references and dereferences and keep only the
        // //           outstanding ones.

        // void                trackMe(bool enable, bool retain);
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
use std::sync::atomic::AtomicBool;
use std::sync::{self, Arc, RwLock};

use crate::{
    binder::*, binder_object::*, error::*, parcel::*,
    parcelable::DeserializeOption,
    process_state::ProcessState, ref_counter::RefCounter, thread_state,
};

/// Cache state for the extension binder object on the proxy side.
enum ExtensionCache {
    /// Remote query has not been performed yet.
    NotQueried,
    /// Remote query completed; stores the result (Some or None).
    Queried(Option<SIBinder>),
}

/// Handle for a proxy to a remote binder service.
///
/// `ProxyHandle` represents the client-side handle to a remote service,
/// managing the connection, transaction routing, and lifecycle events
/// such as death notifications.
pub struct ProxyHandle {
    handle: u32,
    descriptor: String,
    stability: Stability,
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<sync::Weak<dyn DeathRecipient>>>,

    strong: RefCounter,
    weak: RefCounter,
    extension: RwLock<ExtensionCache>,
}

impl ProxyHandle {
    /// Create a new proxy handle for the given binder handle and descriptor.
    /// Takes ownership of the descriptor String to avoid unnecessary allocation.
    pub fn new(handle: u32, descriptor: String, stability: Stability) -> Arc<Self> {
        Arc::new(Self {
            handle,
            descriptor,
            stability,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
            strong: Default::default(),
            weak: Default::default(),
            extension: RwLock::new(ExtensionCache::NotQueried),
        })
    }

    /// Get the underlying binder handle number.
    pub fn handle(&self) -> u32 {
        self.handle
    }

    /// Get the interface descriptor for this proxy.
    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    /// Submit a transaction to the remote service.
    pub fn submit_transact(
        &self,
        code: TransactionCode,
        data: &Parcel,
        flags: TransactionFlags,
    ) -> Result<Option<Parcel>> {
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
        self.obituary_sent
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let recipients = self.recipients.read().expect("Recipients lock poisoned");
        if !recipients.is_empty() {
            thread_state::clear_death_notification(self.handle())?;
            thread_state::flush_commands()?;
        }

        // To remember the recipients to remove
        let mut recipients_to_remove = Vec::new();
        for recipient in recipients.iter() {
            if let Some(recipient) = recipient.upgrade() {
                recipient.binder_died(who);
            } else {
                // The recipient is already dead
                recipients_to_remove.push(recipient.clone());
            }
        }

        drop(recipients); // Release the read lock before acquiring the write lock

        if !recipients_to_remove.is_empty() {
            let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
            for recipient in recipients_to_remove {
                recipients.retain(|r| !sync::Weak::ptr_eq(r, &recipient));
            }
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

impl IBinder for ProxyHandle {
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        // 1. Check cache (read lock)
        {
            let cached = self.extension.read().expect("Extension lock poisoned");
            if let ExtensionCache::Queried(ref ext) = *cached {
                return Ok(ext.clone());
            }
        }

        // 2. Remote query (EXTENSION_TRANSACTION)
        let data = Parcel::new();
        let ext: Option<SIBinder> = match self.submit_transact(EXTENSION_TRANSACTION, &data, 0) {
            Ok(Some(mut reply)) => {
                match DeserializeOption::deserialize_option(&mut reply) {
                    Ok(ext) => ext,
                    Err(_) => return Ok(None),
                }
            }
            _ => return Ok(None),
        };

        // 3. Cache on success only (write lock)
        let mut cache = self.extension.write().expect("Extension lock poisoned");
        *cache = ExtensionCache::Queried(ext.clone());
        Ok(ext)
    }

    fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        let mut ext = self.extension.write().expect("Extension lock poisoned");
        *ext = ExtensionCache::Queried(Some(extension.clone()));
        Ok(())
    }

    /// Register a death notification for this object.
    fn link_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        if self
            .obituary_sent
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(StatusCode::DeadObject);
        } else {
            let mut recipients = self.recipients.write().expect("Recipients lock poisoned");
            if recipients.is_empty() {
                thread_state::request_death_notification(self.handle())?;
                thread_state::flush_commands()?;
            }

            recipients.push(recipient);
        }
        Ok(())
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: sync::Weak<dyn DeathRecipient>) -> Result<()> {
        if self
            .obituary_sent
            .load(std::sync::atomic::Ordering::Relaxed)
        {
            return Err(StatusCode::DeadObject);
        } else {
            let mut recipients = self.recipients.write().expect("Recipients lock poisoned");

            recipients.retain(|r| !sync::Weak::ptr_eq(r, &recipient));
            if recipients.is_empty() {
                thread_state::clear_death_notification(self.handle())?;
                thread_state::flush_commands()?;
            }
        }
        Ok(())
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        thread_state::ping_binder(self.handle())
    }

    // fn stability(&self) -> Stability {
    //     self.stability
    // }

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

    fn inc_strong(&self, strong: &SIBinder) -> Result<()> {
        // In the Android implementation, it simultaneously increases the weak reference,
        // but until the necessity is confirmed, we will not support the related functionality here.
        self.strong
            .inc(|| thread_state::inc_strong_handle(self.handle(), strong.clone()))
    }

    fn attempt_inc_strong(&self) -> bool {
        self.strong.attempt_inc(
            false,
            || {
                if let Err(err) = thread_state::attempt_inc_strong_handle(self.handle()) {
                    log::error!("Error in attempt_inc_strong_handle() is {err:?}");
                    false
                } else {
                    true
                }
            },
            || {
                thread_state::dec_strong_handle(self.handle())
                    .expect("Failed to decrease the binder strong reference count.");
            },
        )
    }

    fn dec_strong(&self, _strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        let handle = self.handle;
        self.strong.dec(|| {
            thread_state::dec_strong_handle(handle)?;

            // Check if both strong and weak counts are reset (INITIAL_STRONG_VALUE)
            // This means no active references exist (except WIBinder's Arc in cache)
            let strong_count = self.strong.count.load(std::sync::atomic::Ordering::Relaxed);
            let weak_count = self.weak.count.load(std::sync::atomic::Ordering::Relaxed);

            if strong_count == crate::ref_counter::INITIAL_STRONG_VALUE
                && weak_count == crate::ref_counter::INITIAL_STRONG_VALUE
            {
                // Both counters are reset - safe to remove from cache
                // This prevents stale proxies when handles are reused (Issue #47)
                ProcessState::as_self().expunge_handle(handle);
            }

            Ok(())
        })
    }

    fn inc_weak(&self, weak: &WIBinder) -> Result<()> {
        self.weak
            .inc(|| thread_state::inc_weak_handle(self.handle(), weak))
    }

    fn dec_weak(&self) -> Result<()> {
        let handle = self.handle;
        self.weak.dec(|| {
            thread_state::dec_weak_handle(handle)?;

            // Check if both strong and weak counts are reset (INITIAL_STRONG_VALUE)
            // This means no active references exist (except WIBinder's Arc in cache)
            let strong_count = self.strong.count.load(std::sync::atomic::Ordering::Relaxed);
            let weak_count = self.weak.count.load(std::sync::atomic::Ordering::Relaxed);

            if strong_count == crate::ref_counter::INITIAL_STRONG_VALUE
                && weak_count == crate::ref_counter::INITIAL_STRONG_VALUE
            {
                // Both counters are reset - safe to remove from cache
                // This prevents stale proxies when handles are reused (Issue #47)
                ProcessState::as_self().expunge_handle(handle);
            }

            Ok(())
        })
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

    #[test]
    fn test_proxy_handle() {
        let handle = ProxyHandle::new(1, "test".to_string(), Stability::Local);
        assert_eq!(handle.handle(), 1);
        assert_eq!(handle.descriptor(), "test");

        assert!(handle.as_transactable().is_none());
        assert!(handle.is_remote());

        // Test for Debug trait
        let debug_str = format!("{handle:?}");
        assert_eq!(
            debug_str,
            "Inner { handle: 1, descriptor: \"test\", stability: Local, obituary_sent: false }"
        );

        // Test for PartialEq trait
        let handle2 = ProxyHandle::new(1, "test".to_string(), Stability::Local);
        assert_eq!(handle, handle2);
    }
}

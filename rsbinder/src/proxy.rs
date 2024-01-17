// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use crate::{
    parcel::*,
    binder::*,
    error::*,
    thread_state,
};

pub struct ProxyHandle {
    handle: u32,
    descriptor: String,
    stability: Stability,
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<Arc<dyn DeathRecipient>>>,
}

impl ProxyHandle {
    pub fn new(handle: u32, descriptor: &str, stability: Stability) -> Self {
        Self {
            handle,
            descriptor: descriptor.to_owned(),
            stability,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
        }
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    pub fn submit_transact(&self, code: TransactionCode, data: &Parcel, flags: TransactionFlags) -> Result<Option<Parcel>> {
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
        self.obituary_sent.store(true, std::sync::atomic::Ordering::Relaxed);

        if !self.recipients.read().unwrap().is_empty() {
            thread_state::clear_death_notification(self.handle())?;
            thread_state::flush_commands()?;
        }

        for recipient in self.recipients.read().unwrap().iter() {
            recipient.binder_died(who);
        }

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
    /// Register a death notification for this object.
    fn link_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        if self.obituary_sent.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        } else {
            if self.recipients.read().unwrap().is_empty() {
                thread_state::request_death_notification(self.handle())?;
                thread_state::flush_commands()?;
            }

            self.recipients.write().unwrap().push(recipient);
        }
        Ok(())
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        if self.obituary_sent.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        } else {
            self.recipients.write().unwrap().retain(|r| !Arc::ptr_eq(r, &recipient));
            if self.recipients.read().unwrap().is_empty() {
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

    fn id(&self) -> u64 {
        self.handle() as u64
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        None
    }
}

pub trait Proxy : Sized + Interface {
    /// The Binder interface descriptor string.
    ///
    /// This string is a unique identifier for a Binder interface, and should be
    /// the same between all implementations of that interface.
    fn descriptor() -> &'static str;

    /// Create a new interface from the given proxy, if it matches the expected
    /// type of this interface.
    fn from_binder(binder: SIBinder) -> Result<Self>;
}

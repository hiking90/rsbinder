// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::sync::Arc;

use crate::{
    parcel::*,
    binder::*,
    error::*,
    thread_state, process_state,
};

#[derive(Debug, Clone, PartialEq)]
pub struct ProxyHandle {
    handle: u32,
    descriptor: String,
    stability: Stability,
}

impl ProxyHandle {
    pub fn new(handle: u32, descriptor: &str, stability: Stability) -> Box<Self> {
        Box::new(Self {
            handle,
            descriptor: descriptor.to_owned(),
            stability,
        })
    }

    pub fn handle(&self) -> u32 {
        self.handle
    }

    pub fn descriptor(&self) -> &str {
        &self.descriptor
    }

    pub fn submit_transact(&self, code: TransactionCode, data: &Parcel, flags: TransactionFlags) -> Result<Option<Parcel>> {
        thread_state::transact(self.handle, code, data, flags)
    }

    pub fn prepare_transact(&self, write_header: bool) -> Result<Parcel> {
        let mut data = Parcel::new();

        if write_header {
            data.write_interface_token(&self.descriptor)?;
        }

        Ok(data)
    }
}

pub(crate) struct ProxyInternal {
    handle: u32,
    weak: WIBinder,
    obituary_sent: bool,
    recipients: Vec<Arc<dyn DeathRecipient>>,
}

impl ProxyInternal {
    pub(crate) fn new(handle: u32, descriptor: &str, stability: Stability) -> Self {
        Self {
            handle,
            weak: WIBinder::new(ProxyHandle::new(handle, descriptor, stability), descriptor),
            obituary_sent: false,
            recipients: Vec::new(),
        }
    }

    pub(crate) fn weak(&self) -> WIBinder {
        self.weak.clone()
    }

    pub(crate) fn link_to_death(&mut self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        if self.obituary_sent {
            return Err(StatusCode::DeadObject);
        } else {
            if self.recipients.is_empty() {
                thread_state::request_death_notification(self.handle)?;
                thread_state::flush_commands()?;
            }
            self.recipients.push(recipient);
        }
        Ok(())
    }

    pub(crate) fn unlink_to_death(&mut self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        if self.obituary_sent {
            return Err(StatusCode::DeadObject);
        } else {
            self.recipients.retain(|r| !Arc::ptr_eq(r, &recipient));
            if self.recipients.is_empty() {
                thread_state::clear_death_notification(self.handle)?;
                thread_state::flush_commands()?;
            }
        }
        Ok(())
    }

    pub(crate) fn send_obituary(&mut self) -> Result<()> {
        self.obituary_sent = true;
        if !self.recipients.is_empty() {
            thread_state::clear_death_notification(self.handle)?;
            thread_state::flush_commands()?;
        }

        for recipient in self.recipients.iter() {
            recipient.binder_died(self.weak.clone());
        }

        Ok(())
    }
}

impl IBinder for ProxyHandle {
    /// Register a death notification for this object.
    fn link_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        process_state::ProcessState::as_self().link_to_death_for_handle(self.handle, recipient)
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        process_state::ProcessState::as_self().unlink_to_death_for_handle(self.handle, recipient)
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        thread_state::ping_binder(self.handle)
    }

    // fn stability(&self) -> Stability {
    //     self.stability
    // }

    fn id(&self) -> u64 {
        self.handle as u64
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

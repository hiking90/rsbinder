// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::any::Any;
use std::fmt::{Debug, Formatter};
use std::mem::ManuallyDrop;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};

use crate::{
    parcel::*,
    binder::*,
    error::*,
    thread_state,
    ref_counter::RefCounter,
    file_descriptor::ParcelFileDescriptor,
};

pub struct ProxyHandle {
    handle: u32,
    descriptor: String,
    stability: Stability,
    obituary_sent: AtomicBool,
    recipients: RwLock<Vec<Arc<dyn DeathRecipient>>>,

    strong: RefCounter,
    weak: RefCounter,
}

impl ProxyHandle {
    pub fn new(handle: u32, descriptor: &str, stability: Stability) -> Arc<Self> {
        Arc::new(Self {
            handle,
            descriptor: descriptor.to_owned(),
            stability,
            obituary_sent: AtomicBool::new(false),
            recipients: RwLock::new(Vec::new()),
            strong: Default::default(),
            weak: Default::default(),
        })
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

        let recipients = self.recipients.read().unwrap();
        if !recipients.is_empty() {
            thread_state::clear_death_notification(self.handle())?;
            thread_state::flush_commands()?;
        }

        for recipient in recipients.iter() {
            recipient.binder_died(who);
        }

        Ok(())
    }

    pub fn dump(&self, writer: &mut dyn WriteExt, args: &[String]) -> Result<()> {
        let fd = writer.as_owned_fd().map_err(|err| {
            log::error!("Failed to as_owned_fd() from writer. {:?}", err);
            StatusCode::BadValue
        })?;
        let file_descriptor = ParcelFileDescriptor::new(fd);
        let mut send = Parcel::new();
        send.write(&file_descriptor)?;
        send.write::<i32>(&(args.len() as i32))?;
        for arg in args {
            send.write(arg)?;
        }
        self.submit_transact(DUMP_TRANSACTION, &send, FLAG_PRIVATE_LOCAL)?;
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
            let mut recipients = self.recipients.write().unwrap();
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
    fn unlink_to_death(&self, recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        if self.obituary_sent.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(StatusCode::DeadObject);
        } else {
            let mut recipients = self.recipients.write().unwrap();

            recipients.retain(|r| !Arc::ptr_eq(r, &recipient));
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
        self.strong.inc(|| {
            thread_state::inc_strong_handle(self.handle(), strong.clone())
        })
    }

    fn attempt_inc_strong(&self) -> bool {
        self.strong.attempt_inc(false, || {
                if let Err(err) = thread_state::attempt_inc_strong_handle(self.handle()) {
                    log::error!("Error in attempt_inc_strong_handle() is {:?}", err);
                    false
                } else {
                    true
                }
            },
            || {
                thread_state::dec_strong_handle(self.handle())
                    .expect("Failed to decrease the binder strong reference count.");
            }
        )
    }

    fn dec_strong(&self, _strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        self.strong.dec(|| {
            thread_state::dec_strong_handle(self.handle())
        })
    }

    fn inc_weak(&self, weak: &WIBinder) -> Result<()> {
        self.weak.inc(|| {
            thread_state::inc_weak_handle(self.handle(), weak)
        })
    }

    fn dec_weak(&self) -> Result<()> {
        self.weak.dec(|| {
            thread_state::dec_weak_handle(self.handle())
        })
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
    fn from_binder(binder: SIBinder) -> Option<Self>;
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! Native service implementation utilities.
//!
//! This module provides helper types and functions for implementing binder services
//! on the server side, including the `Binder` wrapper for native service objects
//! and transaction handling utilities.

use std::any::Any;
use std::convert::TryFrom;
use std::fs::File;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut};
use std::os::fd::FromRawFd;
use std::sync::{Arc, RwLock, Weak};

use crate::{
    binder::*, error::*, parcel::*, parcelable::SerializeOption, ref_counter::RefCounter,
    thread_state,
};

struct Inner<T: Remotable + Send + Sync> {
    remotable: T,
    stability: Stability,
    strong: RefCounter,
    weak: RefCounter,
    extension: RwLock<Option<SIBinder>>,
}

impl<T: Remotable> Inner<T> {
    // The following functions can be redefined depending on the service.
    fn on_transact(
        &self,
        code: TransactionCode,
        _reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        match code {
            INTERFACE_TRANSACTION => reply.write(T::descriptor()),
            DUMP_TRANSACTION => {
                let obj = _reader.read_object(true)?;
                if obj.header_type() != crate::sys::BINDER_TYPE_FD {
                    return Err(StatusCode::BadType);
                }

                let fd = obj.handle();

                let argc = _reader.read::<i32>()?;
                let mut argv = Vec::new();
                for _ in 0..argc {
                    argv.push(_reader.read::<String>()?);
                }

                // SAFETY:
                // 1. The fd comes from a valid flat_binder_object validated by read_object()
                // 2. We use ManuallyDrop because the kernel owns this fd - it will be closed
                //    by the binder driver when the transaction completes
                // 3. The fd is only valid for the duration of this transaction
                // 4. We only use it for writing (dump output), never transfer ownership
                let mut file = unsafe { ManuallyDrop::new(File::from_raw_fd(fd as _)) };

                self.remotable.on_dump(file.deref_mut(), argv.as_slice())
            }
            SHELL_COMMAND_TRANSACTION => {
                log::error!("SHELL_COMMAND_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }
            SYSPROPS_TRANSACTION => {
                log::error!("SYSPROPS_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
}

impl<T: 'static + Remotable> IBinder for Inner<T> {
    fn get_extension(&self) -> Result<Option<SIBinder>> {
        Ok(self
            .extension
            .read()
            .expect("Extension lock poisoned")
            .clone())
    }

    fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        let mut ext = self.extension.write().expect("Extension lock poisoned");
        *ext = Some(extension.clone());
        Ok(())
    }

    fn link_to_death(&self, _recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        log::error!("Binder<T> does not support link_to_death.");
        Err(StatusCode::InvalidOperation)
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, _recipient: Weak<dyn DeathRecipient>) -> Result<()> {
        log::error!("Binder<T> does not support unlink_to_death.");
        Err(StatusCode::InvalidOperation)
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        Ok(())
    }

    fn stability(&self) -> Stability {
        self.stability
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        Some(self)
    }

    fn descriptor(&self) -> &str {
        T::descriptor()
    }

    fn is_remote(&self) -> bool {
        false
    }

    fn inc_strong(&self, _strong: &SIBinder) -> Result<()> {
        self.strong.inc(|| Ok(()))
    }

    fn attempt_inc_strong(&self) -> bool {
        self.strong.attempt_inc(true, || true, || {})
    }

    fn dec_strong(&self, strong: Option<ManuallyDrop<SIBinder>>) -> Result<()> {
        self.strong.dec(|| {
            if let Some(strong) = strong {
                let _ = ManuallyDrop::into_inner(strong);
            }
            Ok(())
        })
    }

    fn inc_weak(&self, _weak: &WIBinder) -> Result<()> {
        self.weak.inc(|| Ok(()))
    }

    fn dec_weak(&self) -> Result<()> {
        self.weak.dec(|| Ok(()))
    }
}

impl<T: Remotable> Transactable for Inner<T> {
    fn transact(
        &self,
        code: TransactionCode,
        reader: &mut Parcel,
        reply: &mut Parcel,
    ) -> Result<()> {
        reader.set_data_position(0);
        match code {
            PING_TRANSACTION => {
                // Noting to do for PING_TRANSACTION.
                Ok(())
            }
            EXTENSION_TRANSACTION => {
                let ext = self.extension.read().expect("Extension lock poisoned");
                SerializeOption::serialize_option(ext.as_ref(), reply)?;
                Ok(())
            }

            STOP_RECORDING_TRANSACTION => {
                log::error!("STOP_RECORDING_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }

            START_RECORDING_TRANSACTION => {
                log::error!("START_RECORDING_TRANSACTION is not supported.");
                Err(StatusCode::InvalidOperation)
            }

            DEBUG_PID_TRANSACTION => {
                reply.write::<i32>(&rustix::process::getpid().as_raw_nonzero().get())
            }

            _ => {
                if (FIRST_CALL_TRANSACTION..=LAST_CALL_TRANSACTION).contains(&code)
                    && !(thread_state::check_interface(reader, T::descriptor())?)
                {
                    reply.write(&StatusCode::BadType)?;
                    return Ok(());
                }

                match self.remotable.on_transact(code, reader, reply) {
                    Ok(_) => Ok(()),
                    Err(err) => {
                        if err == StatusCode::UnknownTransaction {
                            self.on_transact(code, reader, reply)
                        } else {
                            Err(err)
                        }
                    }
                }
            }
        }
    }
}

/// A Binder object that wraps a service implementation for IPC.
///
/// `Binder<T>` provides a wrapper around a service implementation that implements
/// the `Remotable` trait, handling the low-level binder protocol details and
/// dispatching incoming transactions to the appropriate service methods.
pub struct Binder<T: 'static + Remotable + Send + Sync> {
    inner: Arc<Inner<T>>,
}

impl<T: 'static + Remotable> Binder<T> {
    /// Create a new Binder object with default stability.
    pub fn new(remotable: T) -> Self {
        Self::new_with_stability(remotable, Default::default())
    }

    /// Create a new Binder object with the specified stability level.
    pub fn new_with_stability(remotable: T, stability: Stability) -> Self {
        Binder::<T> {
            inner: Arc::new(Inner {
                remotable,
                stability,
                strong: Default::default(),
                weak: Default::default(),
                extension: RwLock::new(None),
            }),
        }
    }
}

impl<T: 'static + Remotable> Binder<T> {
    /// Set the extension binder object.
    pub fn set_extension(&self, extension: &SIBinder) -> Result<()> {
        self.inner.set_extension(extension)
    }

    /// Return the extension binder object, if set.
    pub fn get_extension(&self) -> Result<Option<SIBinder>> {
        self.inner.get_extension()
    }
}

impl<T: 'static + Remotable> Interface for Binder<T> {
    fn as_binder(&self) -> SIBinder {
        SIBinder::new(self.inner.clone()).unwrap_or_else(|e| {
            panic!(
                "Failed to create SIBinder for {}. StatusCode({:?})",
                T::descriptor(),
                e
            )
        })
    }
}

impl<T: Remotable> Clone for Binder<T> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<T: 'static + Remotable> Deref for Binder<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner.remotable
    }
}

// This implementation is an idiomatic implementation of the C++
// `IBinder::localBinder` interface if the binder object is a Rust binder
// service.
impl<B: Remotable + 'static> TryFrom<SIBinder> for Binder<B> {
    type Error = StatusCode;

    fn try_from(ibinder: SIBinder) -> Result<Self> {
        if B::descriptor() != ibinder.descriptor() {
            log::error!(
                "Binder type mismatch: expected {}, got {}",
                B::descriptor(),
                ibinder.descriptor()
            );
            return Err(StatusCode::BadType);
        }

        // SAFETY: We just verified through downcast_ref that this IBinder
        // is actually an Inner<B>, so the cast is safe. The into_raw()
        // gives us the underlying Arc pointer which we reconstruct here.
        if ibinder.as_any().downcast_ref::<Inner<B>>().is_some() {
            let inner_raw = ibinder.into_raw() as *const Inner<B>;
            let inner = unsafe { Arc::from_raw(inner_raw) };

            Ok(Self { inner })
        } else {
            log::error!(
                "Downcast failed: expected {}, got {}",
                B::descriptor(),
                ibinder.descriptor()
            );
            Err(StatusCode::BadValue)
        }
    }
}

/// Determine whether the current thread is currently executing an incoming
/// transaction.
pub fn is_handling_transaction() -> bool {
    thread_state::is_handling_transaction()
}

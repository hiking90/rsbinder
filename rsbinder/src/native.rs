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

use std::sync::Arc;
use std::ops::Deref;
use std::any::Any;
use std::convert::TryFrom;

use crate::binder::*;
use crate::parcel::*;
use crate::error::*;

/// A Binder object that can be used to manage the binder service.
pub struct Binder<T: Remotable + ?Sized + Send + Sync> {
    remotable: Arc<T>,
    stability: Stability,
}

impl<T: Remotable> Binder<T> {
    /// Create a new Binder object.
    pub fn new(remotable: T) -> Self {
        Self::new_with_stability(remotable, Default::default())
    }

    /// Create a new Binder object with the specified stability.
    pub fn new_with_stability(remotable: T, stability: Stability) -> Self {
        Binder {
            remotable: Arc::new(remotable),
            stability,
        }
    }

    /// Retrieve the interface descriptor string for this object's Binder
    /// interface.
    pub fn descriptor(&self) -> &'static str {
        T::descriptor()
    }

    // The following functions can be redefined depending on the service.
    pub fn on_transact(&self, code: TransactionCode, _reader: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            INTERFACE_TRANSACTION => {
                reply.write(self.descriptor())
            }
            DUMP_TRANSACTION => {
                unimplemented!("DUMP_TRANSACTION")
            }
            SHELL_COMMAND_TRANSACTION => {
                unimplemented!("SHELL_COMMAND_TRANSACTION")
            }
            SYSPROPS_TRANSACTION => {
                unimplemented!("SYSPROPS_TRANSACTION")
            }
            _ => Err(StatusCode::UnknownTransaction),
        }
    }
}

impl<T: 'static + Remotable> PartialEq for Binder<T> {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.remotable, &other.remotable)
    }
}

impl<T: 'static + Remotable> Interface for Binder<T> {
    fn as_binder(&self) -> SIBinder {
        SIBinder::new(Box::new((*self).clone()), T::descriptor()).unwrap()
    }
}

impl<T: 'static +  Remotable> IBinder for Binder<T> {
    fn link_to_death(&self, _recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&self, _recipient: Arc<dyn DeathRecipient>) -> Result<()> {
        Err(StatusCode::InvalidOperation)
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        Ok(())
    }

    // /// Retrieve the stability of this object.
    // fn stability(&self) -> Stability {
    //     self.stability
    // }

    fn id(&self) -> u64 {
        Arc::as_ptr(&self.remotable) as u64
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_transactable(&self) -> Option<&dyn Transactable> {
        Some(self)
    }
}

impl<T: Remotable> Transactable for Binder<T> {
    fn transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        reader.set_data_position(0);
        match code {
            PING_TRANSACTION => {
                // Noting to do for PING_TRANSACTION.
                Ok(())
            }
            EXTENSION_TRANSACTION => {
                unimplemented!("EXTENSION_TRANSACTION")
                // CHECK(reply != nullptr);
                // err = reply->writeStrongBinder(getExtension());
            }
            DEBUG_PID_TRANSACTION => {
                unimplemented!("DEBUG_PID_TRANSACTION");
                // CHECK(reply != nullptr);
                // err = reply->writeInt32(getDebugPid());
            }

            _ => {
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

impl<T: Remotable> Clone for Binder<T> {
    fn clone(&self) -> Self {
        Self {
            remotable: self.remotable.clone(),
            stability: self.stability,
        }
    }
}

impl<T: Remotable> Deref for Binder<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.remotable
    }
}

// This implementation is an idiomatic implementation of the C++
// `IBinder::localBinder` interface if the binder object is a Rust binder
// service.
impl<B: Remotable + 'static> TryFrom<SIBinder> for Binder<B> {
    type Error = StatusCode;

    fn try_from(ibinder: SIBinder) -> Result<Self> {
        if B::descriptor() != ibinder.descriptor() {
            return Err(StatusCode::BadType);
        }

        match ibinder.as_any().downcast_ref::<Binder<B>>() {
            Some(binder) => Ok(binder.clone()),
            None => Err(StatusCode::BadType),
        }
    }
}

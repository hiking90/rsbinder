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

use crate::binder::*;
use crate::parcel::*;
use crate::error::*;

pub struct Binder<T: Remotable + ?Sized + Send + Sync> {
    remotable: Arc<T>,
    stability: Stability,
}

impl<T: Remotable> Binder<T> {
    pub fn new(remotable: T) -> Self {
        Self::new_with_stability(remotable, Default::default())
    }

    pub fn new_with_stability(remotable: T, stability: Stability) -> Self {
        Binder {
            remotable: Arc::new(remotable),
            stability,
        }
    }

    pub fn stability(&self) -> Stability {
        self.stability
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

impl<T: 'static + Remotable> Interface for Binder<T> {
    fn as_binder(&self) -> StrongIBinder {
        StrongIBinder::new(Box::new((*self).clone()), T::descriptor())
    }
}

impl<T: 'static +  Remotable> IBinder for Binder<T> {
    fn link_to_death(&mut self, _recipient: &mut dyn DeathRecipient) -> Result<()> {
        todo!("link_to_death")
    }

    /// Remove a previously registered death notification.
    /// The recipient will no longer be called if this object
    /// dies.
    fn unlink_to_death(&mut self, _recipient: &mut dyn DeathRecipient) -> Result<()> {
        todo!("unlink_to_death")
    }

    /// Send a ping transaction to this object
    fn ping_binder(&self) -> Result<()> {
        todo!("ping_binder");
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

// impl<T: Remotable> InterfaceClassMethods for Binder<T> {
//     fn get_descriptor() -> &'static str {
//         <T as Remotable>::get_descriptor()
//     }

//     fn on_create() {

//     }

//     // fn on_transact(
//     //     binder: &mut Binder<T>,
//     //     code: u32,
//     //     data: &parcel::Reader,
//     //     reply: &parcel::Writer,
//     // ) -> Result<()> {
//     //     Ok(())
//     // }

//     fn on_destroy() {
//     }

//     fn on_dump<R: Remotable>(_binder: &mut Binder<R>, _fd: i32, _args: &str, _num_args: u32) -> Result<()> {
//         Ok(())
//     }
// }

// impl<B: Remotable> TryFrom<Object> for Arc<Box<Binder<B>>> {
//     type Error = Error;

//     fn try_from(mut object: Object) -> Result<Self> {
//         match object {
//             Object::Binder { binder, stability } => {
//                 let binder: Self = unsafe { Arc::from_raw(binder as *const Box<Binder<B>>) };
//                 if binder.get_descriptor() == B::get_descriptor() {

//                 }
//             },
//             Object::Handle { .. } => {
//                 Err(Error::from(ErrorKind::BadType))
//             }
//         }
//     }
// }


// // This implementation is an idiomatic implementation of the C++
// // `IBinder::localBinder` interface if the binder object is a Rust binder
// // service.
impl<B: Remotable + 'static> TryFrom<StrongIBinder> for Binder<B> {
    type Error = StatusCode;

    fn try_from(ibinder: StrongIBinder) -> Result<Self> {
        if B::descriptor() != ibinder.descriptor() {
            return Err(StatusCode::BadType);
        }

        match ibinder.as_any().downcast_ref::<Binder<B>>() {
            Some(binder) => Ok(binder.clone()),
            None => Err(StatusCode::BadType),
        }
    }
}

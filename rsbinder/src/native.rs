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

use std::ops::Deref;
use std::any::Any;

use crate::binder::*;
use crate::parcel::*;
use crate::error::*;

pub struct Binder<T: Remotable + ?Sized + Send + Sync> {
    remotable: T,
}

impl<T: Remotable> Binder<T> {
    pub fn new(remotable: T) -> Self {
        Binder {
            remotable,
        }
    }

    /// Retrieve the interface descriptor string for this object's Binder
    /// interface.
    pub fn get_descriptor(&self) -> &'static str {
        T::get_descriptor()
    }

    pub fn transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // data.set_data_position(0);
        match code {
            PING_TRANSACTION => (),
            EXTENSION_TRANSACTION => {
                todo!("EXTENSION_TRANSACTION");
                // CHECK(reply != nullptr);
                // err = reply->writeStrongBinder(getExtension());
            }
            DEBUG_PID_TRANSACTION => {
                todo!("DEBUG_PID_TRANSACTION");
                // CHECK(reply != nullptr);
                // err = reply->writeInt32(getDebugPid());
            }

            _ => {
                self.remotable.on_transact(code, reader, reply)?;
            }
        };

        Ok(())
    }
}

impl<T: 'static + Remotable> Interface for Binder<T> {
    // /// Converts the local remotable object into a generic `SpIBinder`
    // /// reference.
    // ///
    // /// The resulting `SpIBinder` will hold its own strong reference to this
    // /// remotable object, which will prevent the object from being dropped while
    // /// the `SpIBinder` is alive.
    // fn as_any(&self) -> &dyn Any {
    //     self
    // }
    // fn box_clone(&self) -> Box<(dyn Interface + 'static)> {
    //     todo!()
    // }
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

    fn is_remote(&self) -> bool {
        false
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
// impl<B: Remotable> TryFrom<SpIBinder> for Binder<B> {
//     type Error = StatusCode;

//     fn try_from(mut ibinder: SpIBinder) -> Result<Self> {
//         let class = B::get_class();
//         if Some(class) != ibinder.get_class() {
//             return Err(StatusCode::BAD_TYPE);
//         }
//         let userdata = unsafe {
//             // Safety: `SpIBinder` always holds a valid pointer pointer to an
//             // `AIBinder`, which we can safely pass to
//             // `AIBinder_getUserData`. `ibinder` retains ownership of the
//             // returned pointer.
//             sys::AIBinder_getUserData(ibinder.as_native_mut())
//         };
//         if userdata.is_null() {
//             return Err(StatusCode::UNEXPECTED_NULL);
//         }
//         // We are transferring the ownership of the AIBinder into the new Binder
//         // object.
//         let mut ibinder = ManuallyDrop::new(ibinder);
//         Ok(Binder {
//             ibinder: ibinder.as_native_mut(),
//             rust_object: userdata as *mut B,
//         })
//     }
// }

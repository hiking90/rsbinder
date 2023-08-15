// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    fs::File,
};

use crate::{
    binder::*,
    error::*,
    parcel::*,
    parcelable::*,
    thread_state,
};


pub mod transactions {
    use super::*;

    pub const getService: TransactionCode = FIRST_CALL_TRANSACTION + 0;
    pub const checkService: TransactionCode = FIRST_CALL_TRANSACTION + 1;
    pub const addService: TransactionCode = FIRST_CALL_TRANSACTION + 2;
    pub const listServices: TransactionCode = FIRST_CALL_TRANSACTION + 3;
    pub const registerForNotifications: TransactionCode = FIRST_CALL_TRANSACTION + 4;
    pub const unregisterForNotifications: TransactionCode = FIRST_CALL_TRANSACTION + 5;
    pub const isDeclared: TransactionCode = FIRST_CALL_TRANSACTION + 6;
    pub const getDeclaredInstances: TransactionCode = FIRST_CALL_TRANSACTION + 7;
    pub const updatableViaApex: TransactionCode = FIRST_CALL_TRANSACTION + 8;
    pub const registerClientCallback: TransactionCode = FIRST_CALL_TRANSACTION + 9;
    pub const tryUnregisterService: TransactionCode = FIRST_CALL_TRANSACTION + 10;
    pub const getServiceDebugInfo: TransactionCode = FIRST_CALL_TRANSACTION + 11;
}

pub trait IServiceManager: Interface {
    fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceManager" }
    fn get_service(&self, name: &str) -> Result<Option<StrongIBinder>>;
    fn check_service(&self, name: &str) -> Result<Option<StrongIBinder>>;
    fn add_service(&self, name: String, service: StrongIBinder, allow_isolated: bool, dump_priority: i32) -> Result<()>;
}

// lazy_static! {
//     static ref DEFAULT_SERVICE_MANAGER: Arc<dyn IServiceManager> = Arc::new(ProcessState::new());
// }


struct Service {
    binder: StrongIBinder,
    has_clients: bool,
    guarantee_client: bool,
}

pub struct BnServiceManager {
    name_to_service: Mutex<HashMap<String, Service>>,
}

impl BnServiceManager {
    pub fn new() -> Self {
        Self {
            name_to_service: Mutex::new(HashMap::new()),
        }
    }

    fn try_get_service(&self, name: &str, start_if_not_found: bool) -> Option<StrongIBinder> {
        match self.name_to_service.lock().unwrap().get(name) {
            Some(service) => {
                Some(service.binder.clone())
            }
            None => {
                if start_if_not_found == true {
                    log::warn!("{} service could not be found. But, starting the service is not implemented yet.", name);
                }
                None
            },
        }
    }
}

impl Interface for BnServiceManager {
    fn box_clone(&self) -> Box<dyn Interface> {
        todo!()
    }
}

impl IServiceManager for BnServiceManager {
    fn get_service(&self, name: &str) -> Result<Option<StrongIBinder>> {
        Ok(self.try_get_service(name, true))
    }

    fn check_service(&self, name: &str) -> Result<Option<StrongIBinder>> {
        Ok(self.try_get_service(name, false))
    }

    fn add_service(&self, name: String, service: StrongIBinder, _allow_isolated: bool, _dump_priority: i32) -> Result<()> {
        self.name_to_service.lock().unwrap().insert(name, Service {
            binder: service,
            has_clients: false,
            guarantee_client: false,
        });

        Ok(())
    }
}

impl Remotable for BnServiceManager {

    fn get_descriptor() -> &'static str {
        "android.os.IServiceManager"
    }

    fn on_transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        // let mut reader = data.as_readable();
        thread_state::check_interface::<BnServiceManager>(reader)?;

        match code {
            transactions::getService => {
                // let mut reader = data.as_readable();
                // reader.check_interface(Self::get_descriptor())?;
                // self.get_service();
                todo!("transactions::getService");
            }
            transactions::checkService => {
                let name: String16 = reader.read()?;
                let status = self.check_service(&name.0);
                reply.write(&status)?;
                // if let Ok(binder) = status {
                //     writer.write()
                // };
                todo!("transactions::checkService: {} under implementing.", name.0);
            }
            transactions::addService => {
                let name: String16 = reader.read()?;
                let binder: StrongIBinder = reader.read()?;
                let allow_isolated: bool = reader.read()?;
                let dump_priority: i32 = reader.read()?;

                let status = self.add_service(name.0, binder, allow_isolated, dump_priority);

                reply.write(&status)?;
            }
            transactions::listServices => {
                todo!("transactions::listServices");
            }
            transactions::registerForNotifications => {
                todo!("transactions::registerForNotifications");
            }
            transactions::unregisterForNotifications => {
                todo!("transactions::unregisterForNotifications");
            }
            transactions::isDeclared => {
                todo!("transactions::isDeclared");
            }
            transactions::getDeclaredInstances => {
                todo!("transactions::getDeclaredInstances");
            }
            transactions::updatableViaApex => {
                todo!("transactions::updatableViaApex");
            }
            transactions::registerClientCallback => {
                todo!("transactions::registerClientCallback");
            }
            transactions::tryUnregisterService => {
                todo!("transactions::tryUnregisterService");
            }
            transactions::getServiceDebugInfo => {
                todo!("transactions::getServiceDebugInfo");
            }
            _ => {
                println!("Undefined transaction code {:?}", code);
                return Err(ExceptionCode::UnsupportedOperation.into());
            }
        };
        Ok(())
    }

    fn on_dump(&self, _file: &File, _args: &[&str]) -> Result<()> {
        todo!("on_dump()")

    }

    fn get_class<T: InterfaceClassMethods>() -> InterfaceClass<T> {
        todo!("get_class()")

    }
}


pub struct BpServiceManager {
}

impl BpServiceManager {
    pub fn new() -> Self {
        Self {
            // name_to_service: Mutex::new(HashMap::new()),
        }
    }

    // fn try_get_service(&self, name: &str, start_if_not_found: bool) -> Option<Arc<dyn IBinder>> {
    //     match self.name_to_service.lock().unwrap().get(name) {
    //         Some(service) => {
    //             Some(service.binder.clone())
    //         }
    //         None => {
    //             if start_if_not_found == true {
    //                 log::warn!("{} service could not be found. But, starting the service is not implemented yet.", name);
    //             }
    //             None
    //         },
    //     }
    // }
}

impl Interface for BpServiceManager {
    fn box_clone(&self) -> Box<dyn Interface> {
        Box::new(Self {})
    }
}

impl IServiceManager for BpServiceManager {
    fn get_service(&self, name: &str) -> Result<Option<StrongIBinder>> {
        todo!()
    }

    fn check_service(&self, name: &str) -> Result<Option<StrongIBinder>> {
        todo!()
    }

    fn add_service(&self, name: String, service: StrongIBinder, allow_isolated: bool, dump_priority: i32) -> Result<()> {
        todo!()
    }
}
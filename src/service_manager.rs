use std::collections::HashMap;
use std::sync::Arc;
use std::fs::File;

use crate::binder::*;
use crate::error::*;
use crate::parcel::*;

pub trait ServiceManager {
    const TRANSACTION_getService: u32 = FIRST_CALL_TRANSACTION + 0;
    fn get_service(&self, name: &str) -> Option<Arc<Box<dyn IBinder + Send + Sync>>>;

    const TRANSACTION_checkService: u32 = FIRST_CALL_TRANSACTION + 1;
    const TRANSACTION_addService: u32 = FIRST_CALL_TRANSACTION + 2;
    const TRANSACTION_listServices: u32 = FIRST_CALL_TRANSACTION + 3;
    const TRANSACTION_registerForNotifications: u32 = FIRST_CALL_TRANSACTION + 4;
    const TRANSACTION_unregisterForNotifications: u32 = FIRST_CALL_TRANSACTION + 5;
    const TRANSACTION_isDeclared: u32 = FIRST_CALL_TRANSACTION + 6;
    const TRANSACTION_getDeclaredInstances: u32 = FIRST_CALL_TRANSACTION + 7;
    const TRANSACTION_updatableViaApex: u32 = FIRST_CALL_TRANSACTION + 8;
    const TRANSACTION_registerClientCallback: u32 = FIRST_CALL_TRANSACTION + 9;
    const TRANSACTION_tryUnregisterService: u32 = FIRST_CALL_TRANSACTION + 10;
    const TRANSACTION_getServiceDebugInfo: u32 = FIRST_CALL_TRANSACTION + 11;
}

struct Service {
    binder: Arc<Box<dyn IBinder + Sync + Send>>,
    has_clients: bool,
    guarantee_client: bool,
}

pub struct BnServiceManager {
    name_to_service: HashMap<String, Service>,
}

impl BnServiceManager {
    pub fn new() -> Self {
        Self {
            name_to_service: HashMap::new(),
        }
    }
}

impl ServiceManager for BnServiceManager {
    fn get_service(&self, name: &str) -> Option<Arc<Box<dyn IBinder + Send + Sync>>> {
        match self.name_to_service.get(name) {
            Some(service) => {
                Some(service.binder.clone())
            }
            None => None,
        }
    }
}

impl Remotable for BnServiceManager {

    fn get_descriptor() -> &'static str {
        "android.os.IServiceManager"
    }

    fn on_transact(&self, code: TransactionCode, data: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        match code {
            TRANSACTION_getService => {
                // self.get_service();
                todo!("TRANSACTION_getService");
            }
            TRANSACTION_checkService => {
                todo!("TRANSACTION_checkService");
            }
            TRANSACTION_addService => {
                todo!("TRANSACTION_addService");
            }
            TRANSACTION_listServices => {
                todo!("TRANSACTION_listServices");
            }
            TRANSACTION_registerForNotifications => {
                todo!("TRANSACTION_registerForNotifications");
            }
            TRANSACTION_unregisterForNotifications => {
                todo!("TRANSACTION_unregisterForNotifications");
            }
            TRANSACTION_isDeclared => {
                todo!("TRANSACTION_isDeclared");
            }
            TRANSACTION_getDeclaredInstances => {
                todo!("TRANSACTION_getDeclaredInstances");
            }
            TRANSACTION_updatableViaApex => {
                todo!("TRANSACTION_updatableViaApex");
            }
            TRANSACTION_registerClientCallback => {
                todo!("TRANSACTION_registerClientCallback");
            }
            TRANSACTION_tryUnregisterService => {
                todo!("TRANSACTION_tryUnregisterService");
            }
            TRANSACTION_getServiceDebugInfo => {
                todo!("TRANSACTION_getServiceDebugInfo");
            }
            _ => {
                return Err(Error::from(ErrorKind::Unknown));
            }
        };
        Ok(())
    }

    fn on_dump(&self, file: &File, args: &[&str]) -> Result<()> {
        todo!("on_dump()")

    }

    fn get_class<T: InterfaceClassMethods>() -> InterfaceClass<T> {
        todo!("get_class()")

    }
}

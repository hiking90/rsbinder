// Copyright 2026 Jyotiraditya Panda <jyotiraditya@aospa.co>
// SPDX-License-Identifier: Apache-2.0

use crate::*;

// Interface descriptor for the Android 10 C service manager.
pub const SERVICE_MANAGER_DESCRIPTOR: &str = "android.os.IServiceManager";

// Re-export the priority flags from the hub root so values can never drift
// from the AIDL-generated constants used by API 30+.
pub use crate::hub::{
    DUMP_FLAG_PRIORITY_ALL, DUMP_FLAG_PRIORITY_CRITICAL, DUMP_FLAG_PRIORITY_DEFAULT,
    DUMP_FLAG_PRIORITY_HIGH, DUMP_FLAG_PRIORITY_NORMAL,
};
pub const DUMP_FLAG_PROTO: i32 = 1 << 4;

// `addService` allow_isolated argument; the C service manager treats any
// non-zero value as true.
const ALLOW_ISOLATED_FALSE: i32 = 0;

// Transaction codes used by the Android 10 C service manager.
const GET_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION;
const CHECK_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION + 1;
const ADD_SERVICE: TransactionCode = FIRST_CALL_TRANSACTION + 2;
const LIST_SERVICES: TransactionCode = FIRST_CALL_TRANSACTION + 3;

/// Client proxy for the Android 10 service manager.
pub struct BpServiceManager {
    binder: SIBinder,
}

impl BpServiceManager {
    pub fn from_binder(binder: SIBinder) -> Option<Self> {
        if binder.as_proxy().is_some() {
            Some(Self { binder })
        } else {
            None
        }
    }

    fn proxy(&self) -> &proxy::ProxyHandle {
        self.binder
            .as_proxy()
            .expect("BpServiceManager must wrap a proxy binder")
    }

    fn transact(&self, code: TransactionCode, data: &Parcel) -> Result<Parcel> {
        self.proxy()
            .submit_transact(code, data, FLAG_CLEAR_BUF)?
            .ok_or(StatusCode::UnexpectedNull)
    }
}

/// Retrieve an existing service, blocking for a few seconds if it doesn't yet
/// exist.
pub fn get_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
    let result = (|| -> Result<Option<SIBinder>> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(name)?;
        sm.transact(GET_SERVICE, &data)?.read()
    })();

    match result {
        Ok(binder) => binder,
        Err(err) => {
            log::error!("Failed to get service {name}: {err:?}");
            None
        }
    }
}

/// Retrieve an existing service called @a name from the service
/// manager. Non-blocking. Returns null if the service does not
/// exist.
pub fn check_service(sm: &BpServiceManager, name: &str) -> Option<SIBinder> {
    let result = (|| -> Result<Option<SIBinder>> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(name)?;
        sm.transact(CHECK_SERVICE, &data)?.read()
    })();

    match result {
        Ok(binder) => binder,
        Err(err) => {
            log::error!("Failed to check service {name}: {err}");
            None
        }
    }
}

/// Return a list of all currently running services.
/// Iterates one entry at a time using an index, unlike API 30+.
pub fn list_services(sm: &BpServiceManager, dump_priority: i32) -> Vec<String> {
    let mut services = Vec::new();
    let mut n: i32 = 0;

    loop {
        let result = (|| -> Result<String> {
            let mut data = sm.proxy().prepare_transact(true)?;
            data.write::<i32>(&n)?;
            data.write::<i32>(&dump_priority)?;
            sm.transact(LIST_SERVICES, &data)?.read::<String>()
        })();

        match result {
            Ok(name) => {
                services.push(name);
                n += 1;
            }
            Err(err) => {
                // Termination is signalled by an error after the last
                // entry, so a failure on n>0 is the normal end-of-list
                // path. n==0 means we never read a single entry — that
                // is a real IPC failure worth surfacing.
                if n == 0 {
                    log::error!("Failed to list services: {err}");
                }
                break;
            }
        }
    }

    services
}

pub fn add_service(
    sm: &BpServiceManager,
    identifier: &str,
    binder: SIBinder,
) -> std::result::Result<(), Status> {
    let result = (|| -> Result<()> {
        let mut data = sm.proxy().prepare_transact(true)?;
        data.write(identifier)?;
        data.write(&binder)?;
        data.write::<i32>(&ALLOW_ISOLATED_FALSE)?;
        data.write::<i32>(&DUMP_FLAG_PRIORITY_DEFAULT)?;

        let mut reply = sm.transact(ADD_SERVICE, &data)?;
        let status = reply.read::<Status>()?;
        if !status.is_ok() {
            return Err(StatusCode::from(status));
        }

        Ok(())
    })();

    result.map_err(Status::from)
}

pub fn get_interface<T: FromIBinder + ?Sized>(
    sm: &BpServiceManager,
    name: &str,
) -> Result<Strong<T>> {
    match get_service(sm, name) {
        Some(service) => FromIBinder::try_from(service),
        None => {
            log::error!("Service {name} not found");
            Err(StatusCode::NameNotFound)
        }
    }
}

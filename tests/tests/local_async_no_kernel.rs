// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 17 D-10: in a process without kernel binder (an RPC-only process: `ProcessState` is
//! never initialized), a local binder that would be called through the in-process transport is
//! refused at cast time with `InvalidOperation`, and `into_async` panics naming the reason.
//!
//! Nothing in this binary initializes `ProcessState`, so it needs no binder device.

use std::panic::{catch_unwind, AssertUnwindSafe};

use rsbinder::{
    BinderResult, Interface, ParcelFileDescriptor, SIBinder, StatusCode, Strong, Tokio,
};

include!(concat!(env!("OUT_DIR"), "/local_async.rs"));

use localasync::ILocalSvc::{BnLocalSvc, ILocalSvc, ILocalSvcAsync};

struct Svc;

impl Interface for Svc {}

impl ILocalSvc for Svc {
    fn r#callerKind(&self) -> BinderResult<String> {
        Ok("none".to_string())
    }
    fn r#handlingTransaction(&self) -> BinderResult<bool> {
        Ok(false)
    }
    fn r#threadId(&self) -> BinderResult<String> {
        Ok(String::new())
    }
    fn r#threadState(&self) -> BinderResult<String> {
        Ok(String::new())
    }
    fn r#echoBinder(&self, b: &SIBinder) -> BinderResult<SIBinder> {
        Ok(b.clone())
    }
    fn r#readFd(&self, _fd: &ParcelFileDescriptor) -> BinderResult<String> {
        Ok(String::new())
    }
    fn r#bump(&self) -> BinderResult<()> {
        Ok(())
    }
    fn r#bumps(&self) -> BinderResult<i32> {
        Ok(0)
    }
    fn r#guarded(&self) -> BinderResult<bool> {
        Ok(true)
    }
}

#[test]
fn async_cast_of_a_sync_only_service_is_invalid_operation_without_kernel_binder() {
    assert!(!rsbinder::ProcessState::is_initialized());
    let sync: Strong<dyn ILocalSvc> = BnLocalSvc::new_binder(Svc);
    assert_eq!(
        sync.as_binder()
            .into_interface::<dyn ILocalSvcAsync<Tokio>>()
            .err(),
        Some(StatusCode::InvalidOperation)
    );
    assert_eq!(
        sync.clone().try_into_async::<Tokio>().err(),
        Some(StatusCode::InvalidOperation)
    );
    // The sync handle is a direct call and needs no kernel binder.
    assert_eq!(sync.callerKind(), Ok("none".to_string()));

    let Err(panic) = catch_unwind(AssertUnwindSafe(|| sync.into_async::<Tokio>())) else {
        panic!("into_async panics");
    };
    let message = panic
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .expect("a panic message");
    assert!(
        message.contains("without kernel binder") && message.contains("try_into_async"),
        "{message}"
    );
}

#[test]
fn cast_of_another_remotable_is_invalid_operation_without_kernel_binder() {
    struct Alias;
    impl rsbinder::Remotable for Alias {
        fn descriptor() -> &'static str {
            "localasync.ILocalSvc"
        }
        fn on_transact(
            &self,
            _: rsbinder::TransactionCode,
            _: &mut rsbinder::Parcel,
            _: &mut rsbinder::Parcel,
        ) -> rsbinder::Result<()> {
            Err(StatusCode::UnknownTransaction)
        }
        fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> rsbinder::Result<()> {
            Ok(())
        }
    }

    let binder = rsbinder::Binder::new(Alias).as_binder();
    assert_eq!(
        binder.clone().into_interface::<dyn ILocalSvc>().err(),
        Some(StatusCode::InvalidOperation)
    );
    assert_eq!(
        binder.into_interface::<dyn ILocalSvcAsync<Tokio>>().err(),
        Some(StatusCode::InvalidOperation)
    );
}

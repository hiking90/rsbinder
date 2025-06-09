#![allow(non_snake_case, dead_code, unused_imports, unused_macros)]

use env_logger::Env;

pub use rsbinder::*;

include!(concat!(env!("OUT_DIR"), "/test_aidl.rs"));

pub(crate) use android::aidl::tests::sm::IFoo::{IFoo, BnFoo, BpFoo};

pub(crate) struct IFooService {
}

impl rsbinder::Interface for IFooService {
}

impl IFoo for IFooService {
    // Implement the echo method.
    fn hello(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

use super::*;
use std::sync::OnceLock;

fn setup() {
    // static INIT: OnceLock<bool> = OnceLock::new();

    // let _ = INIT.get_or_init(|| {
    //     env_logger::init();
    //     rsbinder::ProcessState::init(rsbinder::DEFAULT_BINDER_PATH, 0);
    //     true
    // });
}

#[test]
fn test_add_service() -> rsbinder::Result<()> {
    setup();

    let service = BnFoo::new_binder(IFooService{});
    assert!(hub::add_service("", service.as_binder()).is_err());

    assert_eq!(hub::add_service("foo", service.as_binder()), Ok(()));

    // The maximum length of service name is 127.
    let s = std::iter::repeat('a').take(127).collect::<String>();
    assert!(hub::add_service(&s, service.as_binder()).is_ok());

    let s = std::iter::repeat('a').take(128).collect::<String>();
    assert!(hub::add_service(&s, service.as_binder()).is_err());

    // Weird characters are not allowed.
    assert!(hub::add_service("happy$foo$fo", service.as_binder()).is_err());

    // Overwrite the service
    assert_eq!(hub::add_service("foo", service.as_binder()), Ok(()));

    Ok(())
}

#[test]
fn test_get_check_list_service() -> rsbinder::Result<()> {
    setup();

    #[cfg(target_os = "android")]
    {
        let manager_name = "manager";
        let binder = hub::get_service(manager_name);
        assert!(binder.is_some());

        let binder = hub::check_service(manager_name);
        assert!(binder.is_some());
    }

    let unknown_name = "unknown_service";
    let binder = hub::get_service(unknown_name);
    assert!(binder.is_none());
    let binder = hub::check_service(unknown_name);
    assert!(binder.is_none());

    let services = hub::list_services(hub::DUMP_FLAG_PRIORITY_DEFAULT);
    assert!(!services.is_empty());

    Ok(())
}

#[test]
fn test_notifications() -> rsbinder::Result<()> {
    setup();

    struct MyServiceCallback {}
    impl rsbinder::Interface for MyServiceCallback {}
    impl hub::IServiceCallback for MyServiceCallback {
        fn onRegistration(&self, name: &str, service: &rsbinder::SIBinder) -> rsbinder::status::Result<()> {
            println!("onRegistration: {} {:?}", name, service);
            Ok(())
        }
    }

    let callback = hub::BnServiceCallback::new_binder(MyServiceCallback{});

    hub::register_for_notifications("mytest_service", &callback)?;
    hub::unregister_for_notifications("mytest_service", &callback)?;

    Ok(())
}

#[test]
fn test_others() -> rsbinder::Result<()> {
    setup();

    assert!(!hub::is_declared("android.debug.IAdbManager/default"));

    Ok(())
}

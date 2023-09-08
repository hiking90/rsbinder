use rsbinder_aidl::Builder;
use std::path::PathBuf;
use std::error::Error;
use similar::{ChangeTag, TextDiff};
use rsbinder_aidl;

#[test]
fn test_service_manager() -> Result<(), Box<dyn Error>> {
    Builder::new()
        .source(PathBuf::from("../aidl/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("../aidl/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("../aidl/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("../aidl/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("../aidl/android/os/ServiceDebugInfo.aidl"))
        .generate()?;

    Ok(())
}

// #[test]
// fn test_builder() -> Result<(), Box<dyn Error>> {
//     Builder::new()
//         .source(PathBuf::from("aidl"))
//         .generate()
// }

fn aidl_generator(input: &str, expect: &str) -> Result<(), Box<dyn Error>> {
    let document = rsbinder_aidl::parse_document(input)?;
    let res = rsbinder_aidl::gen_document(&document)?;
    let diff = TextDiff::from_lines(res.1.trim(), expect.trim());
    for change in diff.iter_all_changes() {
        let sign = match change.tag() {
            ChangeTag::Delete => "- ",
            ChangeTag::Insert => "+ ",
            ChangeTag::Equal => "  ",
        };
        print!("{}{}", sign, change);
    }
    assert_eq!(res.1.trim(), expect.trim());
    Ok(())
}

#[test]
fn test_interface() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
package android.os;

parcelable ConnectionInfo {
    @utf8InCpp String ipAddress;
    int port;
}

oneway interface IClientCallback {
    void onClients(IBinder registered, boolean hasClients);
}

oneway interface IServiceCallback {
    void onRegistration(@utf8InCpp String name, IBinder binder);
}

interface IServiceManager {
    const int DUMP_FLAG_PRIORITY_CRITICAL = 1 << 0;
    const int DUMP_FLAG_PRIORITY_HIGH = 1 << 1;
    const int DUMP_FLAG_PRIORITY_NORMAL = 1 << 2;
    const int DUMP_FLAG_PRIORITY_DEFAULT = 1 << 3;
    const int DUMP_FLAG_PRIORITY_ALL =
             DUMP_FLAG_PRIORITY_CRITICAL | DUMP_FLAG_PRIORITY_HIGH
             | DUMP_FLAG_PRIORITY_NORMAL | DUMP_FLAG_PRIORITY_DEFAULT;
    const int DUMP_FLAG_PROTO = 1 << 4;

    @UnsupportedAppUsage
    @nullable IBinder getService(@utf8InCpp String name);
    @UnsupportedAppUsage
    @nullable IBinder checkService(@utf8InCpp String name);
    void addService(@utf8InCpp String name, IBinder service,
        boolean allowIsolated, int dumpPriority);
    @utf8InCpp String[] listServices(int dumpPriority);
    void registerForNotifications(@utf8InCpp String name, IServiceCallback callback);
    void unregisterForNotifications(@utf8InCpp String name, IServiceCallback callback);
    boolean isDeclared(@utf8InCpp String name);
    @utf8InCpp String[] getDeclaredInstances(@utf8InCpp String iface);
    @nullable @utf8InCpp String updatableViaApex(@utf8InCpp String name);
    @nullable ConnectionInfo getConnectionInfo(@utf8InCpp String name);
    void registerClientCallback(@utf8InCpp String name, IBinder service, IClientCallback callback);
    void tryUnregisterService(@utf8InCpp String name, IBinder service);
    ServiceDebugInfo[] getServiceDebugInfo();
}

parcelable ServiceDebugInfo {
    @utf8InCpp String name;
    int debugPid;
}
            "##, r##"
pub use connection_info::*;
mod connection_info {
    #[derive(Debug, Default)]
    pub struct ConnectionInfo {
        pub ip_address: String,
        pub port: i32,
    }
    impl Default for ConnectionInfo {
        fn default() -> Self {
            Self {
                ip_address: Default::default(),
                port: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for ConnectionInfo {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.write(&self.ip_address)?;
            _parcel.write(&self.port)?;
            Ok(())
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            self.ip_address = _parcel.read()?;
            self.port = _parcel.read()?;
            Ok(())
        }
    }
    rsbinder::impl_serialize_for_parcelable!(ConnectionInfo);
    rsbinder::impl_deserialize_for_parcelable!(ConnectionInfo);
    impl rsbinder::ParcelableMetadata for ConnectionInfo {
        fn get_descriptor() -> &'static str { "android.os.ConnectionInfo" }
    }
}
pub use i_client_callback::*;
mod i_client_callback {
    pub trait IClientCallback: rsbinder::Interface + Send {
        fn on_clients(&self, _arg_registered: rsbinder::StrongIBinder, _arg_has_clients: bool) -> rsbinder::Result<()>;
    }
    pub(crate) mod transactions {
        pub(crate) const ON_CLIENTS: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
    }
    rsbinder::declare_binder_interface! {
        IClientCallback["android.os.IClientCallback"] {
            native: BnClientCallback(on_transact),
            proxy: BpClientCallback,
        }
    }
    impl BpClientCallback {
        fn build_parcel_on_clients(&self, _arg_registered: rsbinder::StrongIBinder, _arg_has_clients: bool) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(&_arg_registered)?;
            data.write(&_arg_has_clients)?;
            Ok(data)
        }
        fn read_response_on_clients(&self, _arg_registered: rsbinder::StrongIBinder, _arg_has_clients: bool, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            Ok(())
        }
    }
    impl IClientCallback for BpClientCallback {
        fn on_clients(&self, _arg_registered: rsbinder::StrongIBinder, _arg_has_clients: bool) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_on_clients(_arg_registered.clone(), _arg_has_clients, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::ON_CLIENTS, &_aidl_data, rsbinder::FLAG_ONEWAY | rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_on_clients(_arg_registered, _arg_has_clients, _aidl_reply)
        }
    }
    fn on_transact(
        _service: &dyn IClientCallback, _code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {
        Ok(())
    }
}
pub use i_service_callback::*;
mod i_service_callback {
    pub trait IServiceCallback: rsbinder::Interface + Send {
        fn on_registration(&self, _arg_name: &str, _arg_binder: rsbinder::StrongIBinder) -> rsbinder::Result<()>;
    }
    pub(crate) mod transactions {
        pub(crate) const ON_REGISTRATION: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
    }
    rsbinder::declare_binder_interface! {
        IServiceCallback["android.os.IServiceCallback"] {
            native: BnServiceCallback(on_transact),
            proxy: BpServiceCallback,
        }
    }
    impl BpServiceCallback {
        fn build_parcel_on_registration(&self, _arg_name: &str, _arg_binder: rsbinder::StrongIBinder) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(&_arg_binder)?;
            Ok(data)
        }
        fn read_response_on_registration(&self, _arg_name: &str, _arg_binder: rsbinder::StrongIBinder, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            Ok(())
        }
    }
    impl IServiceCallback for BpServiceCallback {
        fn on_registration(&self, _arg_name: &str, _arg_binder: rsbinder::StrongIBinder) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_on_registration(_arg_name, _arg_binder.clone(), )?;
            let _aidl_reply = self.handle.submit_transact(transactions::ON_REGISTRATION, &_aidl_data, rsbinder::FLAG_ONEWAY | rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_on_registration(_arg_name, _arg_binder, _aidl_reply)
        }
    }
    fn on_transact(
        _service: &dyn IServiceCallback, _code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {
        Ok(())
    }
}
pub use i_service_manager::*;
mod i_service_manager {
    pub const DUMP_FLAG_PRIORITY_CRITICAL: i32 = 1;
    pub const DUMP_FLAG_PRIORITY_HIGH: i32 = 2;
    pub const DUMP_FLAG_PRIORITY_NORMAL: i32 = 4;
    pub const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 8;
    pub const DUMP_FLAG_PRIORITY_ALL: i32 = 15;
    pub const DUMP_FLAG_PROTO: i32 = 16;
    pub trait IServiceManager: rsbinder::Interface + Send {
        fn get_service(&self, _arg_name: &str) -> rsbinder::Result<Option<rsbinder::StrongIBinder>>;
        fn check_service(&self, _arg_name: &str) -> rsbinder::Result<Option<rsbinder::StrongIBinder>>;
        fn add_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_allow_isolated: bool, _arg_dump_priority: i32) -> rsbinder::Result<()>;
        fn list_services(&self, _arg_dump_priority: i32) -> rsbinder::Result<Vec<String>>;
        fn register_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<()>;
        fn unregister_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<()>;
        fn is_declared(&self, _arg_name: &str) -> rsbinder::Result<bool>;
        fn get_declared_instances(&self, _arg_iface: &str) -> rsbinder::Result<Vec<String>>;
        fn updatable_via_apex(&self, _arg_name: &str) -> rsbinder::Result<Option<String>>;
        fn get_connection_info(&self, _arg_name: &str) -> rsbinder::Result<Option<crate::aidl::android::os::ConnectionInfo>>;
        fn register_client_callback(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IClientCallback>) -> rsbinder::Result<()>;
        fn try_unregister_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder) -> rsbinder::Result<()>;
        fn get_service_debug_info(&self) -> rsbinder::Result<Vec<crate::aidl::android::os::ServiceDebugInfo>>;
    }
    pub(crate) mod transactions {
        pub(crate) const GET_SERVICE: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 0;
        pub(crate) const CHECK_SERVICE: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 1;
        pub(crate) const ADD_SERVICE: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 2;
        pub(crate) const LIST_SERVICES: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 3;
        pub(crate) const REGISTER_FOR_NOTIFICATIONS: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 4;
        pub(crate) const UNREGISTER_FOR_NOTIFICATIONS: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 5;
        pub(crate) const IS_DECLARED: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 6;
        pub(crate) const GET_DECLARED_INSTANCES: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 7;
        pub(crate) const UPDATABLE_VIA_APEX: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 8;
        pub(crate) const GET_CONNECTION_INFO: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 9;
        pub(crate) const REGISTER_CLIENT_CALLBACK: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 10;
        pub(crate) const TRY_UNREGISTER_SERVICE: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 11;
        pub(crate) const GET_SERVICE_DEBUG_INFO: rsbinder::TransactionCode = rsbinder::FIRST_CALL_TRANSACTION + 12;
    }
    rsbinder::declare_binder_interface! {
        IServiceManager["android.os.IServiceManager"] {
            native: BnServiceManager(on_transact),
            proxy: BpServiceManager,
        }
    }
    impl BpServiceManager {
        fn build_parcel_get_service(&self, _arg_name: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            Ok(data)
        }
        fn read_response_get_service(&self, _arg_name: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Option<rsbinder::StrongIBinder>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Option<rsbinder::StrongIBinder> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_check_service(&self, _arg_name: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            Ok(data)
        }
        fn read_response_check_service(&self, _arg_name: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Option<rsbinder::StrongIBinder>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Option<rsbinder::StrongIBinder> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_add_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_allow_isolated: bool, _arg_dump_priority: i32) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(&_arg_service)?;
            data.write(&_arg_allow_isolated)?;
            data.write(&_arg_dump_priority)?;
            Ok(data)
        }
        fn read_response_add_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_allow_isolated: bool, _arg_dump_priority: i32, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
        }
        fn build_parcel_list_services(&self, _arg_dump_priority: i32) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(&_arg_dump_priority)?;
            Ok(data)
        }
        fn read_response_list_services(&self, _arg_dump_priority: i32, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Vec<String>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Vec<String> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_register_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(_arg_callback.as_ref())?;
            Ok(data)
        }
        fn read_response_register_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
        }
        fn build_parcel_unregister_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(_arg_callback.as_ref())?;
            Ok(data)
        }
        fn read_response_unregister_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
        }
        fn build_parcel_is_declared(&self, _arg_name: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            Ok(data)
        }
        fn read_response_is_declared(&self, _arg_name: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<bool> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: bool = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_get_declared_instances(&self, _arg_iface: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_iface)?;
            Ok(data)
        }
        fn read_response_get_declared_instances(&self, _arg_iface: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Vec<String>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Vec<String> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_updatable_via_apex(&self, _arg_name: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            Ok(data)
        }
        fn read_response_updatable_via_apex(&self, _arg_name: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Option<String>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Option<String> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_get_connection_info(&self, _arg_name: &str) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            Ok(data)
        }
        fn read_response_get_connection_info(&self, _arg_name: &str, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Option<crate::aidl::android::os::ConnectionInfo>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Option<crate::aidl::android::os::ConnectionInfo> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
        fn build_parcel_register_client_callback(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IClientCallback>) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(&_arg_service)?;
            data.write(_arg_callback.as_ref())?;
            Ok(data)
        }
        fn read_response_register_client_callback(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IClientCallback>, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
        }
        fn build_parcel_try_unregister_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder) -> rsbinder::Result<rsbinder::Parcel> {
            let mut data = self.handle.prepare_transact(true)?;
            data.write(_arg_name)?;
            data.write(&_arg_service)?;
            Ok(data)
        }
        fn read_response_try_unregister_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<()> {
            let _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            Ok(())
        }
        fn build_parcel_get_service_debug_info(&self) -> rsbinder::Result<rsbinder::Parcel> {
            let data = self.handle.prepare_transact(true)?;
            Ok(data)
        }
        fn read_response_get_service_debug_info(&self, _aidl_reply: Option<rsbinder::Parcel>) -> rsbinder::Result<Vec<crate::aidl::android::os::ServiceDebugInfo>> {
            let mut _aidl_reply = _aidl_reply.unwrap();
            let _status = _aidl_reply.read::<rsbinder::Status>()?;
            let _aidl_return: Vec<crate::aidl::android::os::ServiceDebugInfo> = _aidl_reply.read()?;
            Ok(_aidl_return)
        }
    }
    impl IServiceManager for BpServiceManager {
        fn get_service(&self, _arg_name: &str) -> rsbinder::Result<Option<rsbinder::StrongIBinder>> {
            let _aidl_data = self.build_parcel_get_service(_arg_name, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::GET_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_get_service(_arg_name, _aidl_reply)
        }
        fn check_service(&self, _arg_name: &str) -> rsbinder::Result<Option<rsbinder::StrongIBinder>> {
            let _aidl_data = self.build_parcel_check_service(_arg_name, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::CHECK_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_check_service(_arg_name, _aidl_reply)
        }
        fn add_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_allow_isolated: bool, _arg_dump_priority: i32) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_add_service(_arg_name, _arg_service.clone(), _arg_allow_isolated, _arg_dump_priority, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::ADD_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_add_service(_arg_name, _arg_service, _arg_allow_isolated, _arg_dump_priority, _aidl_reply)
        }
        fn list_services(&self, _arg_dump_priority: i32) -> rsbinder::Result<Vec<String>> {
            let _aidl_data = self.build_parcel_list_services(_arg_dump_priority, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::LIST_SERVICES, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_list_services(_arg_dump_priority, _aidl_reply)
        }
        fn register_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_register_for_notifications(_arg_name, _arg_callback.clone(), )?;
            let _aidl_reply = self.handle.submit_transact(transactions::REGISTER_FOR_NOTIFICATIONS, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_register_for_notifications(_arg_name, _arg_callback, _aidl_reply)
        }
        fn unregister_for_notifications(&self, _arg_name: &str, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IServiceCallback>) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_unregister_for_notifications(_arg_name, _arg_callback.clone(), )?;
            let _aidl_reply = self.handle.submit_transact(transactions::UNREGISTER_FOR_NOTIFICATIONS, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_unregister_for_notifications(_arg_name, _arg_callback, _aidl_reply)
        }
        fn is_declared(&self, _arg_name: &str) -> rsbinder::Result<bool> {
            let _aidl_data = self.build_parcel_is_declared(_arg_name, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::IS_DECLARED, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_is_declared(_arg_name, _aidl_reply)
        }
        fn get_declared_instances(&self, _arg_iface: &str) -> rsbinder::Result<Vec<String>> {
            let _aidl_data = self.build_parcel_get_declared_instances(_arg_iface, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::GET_DECLARED_INSTANCES, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_get_declared_instances(_arg_iface, _aidl_reply)
        }
        fn updatable_via_apex(&self, _arg_name: &str) -> rsbinder::Result<Option<String>> {
            let _aidl_data = self.build_parcel_updatable_via_apex(_arg_name, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::UPDATABLE_VIA_APEX, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_updatable_via_apex(_arg_name, _aidl_reply)
        }
        fn get_connection_info(&self, _arg_name: &str) -> rsbinder::Result<Option<crate::aidl::android::os::ConnectionInfo>> {
            let _aidl_data = self.build_parcel_get_connection_info(_arg_name, )?;
            let _aidl_reply = self.handle.submit_transact(transactions::GET_CONNECTION_INFO, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_get_connection_info(_arg_name, _aidl_reply)
        }
        fn register_client_callback(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder, _arg_callback: std::sync::Arc<dyn crate::aidl::android::os::IClientCallback>) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_register_client_callback(_arg_name, _arg_service.clone(), _arg_callback.clone(), )?;
            let _aidl_reply = self.handle.submit_transact(transactions::REGISTER_CLIENT_CALLBACK, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_register_client_callback(_arg_name, _arg_service, _arg_callback, _aidl_reply)
        }
        fn try_unregister_service(&self, _arg_name: &str, _arg_service: rsbinder::StrongIBinder) -> rsbinder::Result<()> {
            let _aidl_data = self.build_parcel_try_unregister_service(_arg_name, _arg_service.clone(), )?;
            let _aidl_reply = self.handle.submit_transact(transactions::TRY_UNREGISTER_SERVICE, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_try_unregister_service(_arg_name, _arg_service, _aidl_reply)
        }
        fn get_service_debug_info(&self) -> rsbinder::Result<Vec<crate::aidl::android::os::ServiceDebugInfo>> {
            let _aidl_data = self.build_parcel_get_service_debug_info()?;
            let _aidl_reply = self.handle.submit_transact(transactions::GET_SERVICE_DEBUG_INFO, &_aidl_data, rsbinder::FLAG_PRIVATE_VENDOR)?;
            self.read_response_get_service_debug_info(_aidl_reply)
        }
    }
    fn on_transact(
        _service: &dyn IServiceManager, _code: rsbinder::TransactionCode,) -> rsbinder::Result<()> {
        Ok(())
    }
}
pub use service_debug_info::*;
mod service_debug_info {
    #[derive(Debug, Default)]
    pub struct ServiceDebugInfo {
        pub name: String,
        pub debug_pid: i32,
    }
    impl Default for ServiceDebugInfo {
        fn default() -> Self {
            Self {
                name: Default::default(),
                debug_pid: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for ServiceDebugInfo {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.write(&self.name)?;
            _parcel.write(&self.debug_pid)?;
            Ok(())
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            self.name = _parcel.read()?;
            self.debug_pid = _parcel.read()?;
            Ok(())
        }
    }
    rsbinder::impl_serialize_for_parcelable!(ServiceDebugInfo);
    rsbinder::impl_deserialize_for_parcelable!(ServiceDebugInfo);
    impl rsbinder::ParcelableMetadata for ServiceDebugInfo {
        fn get_descriptor() -> &'static str { "android.os.ServiceDebugInfo" }
    }
}            "##)
}

#[test]
fn test_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
        package android.os;

        /**
         * Remote connection info associated with a declared service
         * @hide
         */
        parcelable ConnectionInfo {
            /**
             * IP address that the service is listening on.
             */
            @utf8InCpp String ipAddress;
            /**
             * Port number that the service is listening on. Actual value is an unsigned integer.
             */
            int port;
        }
        "##, r##"
pub use connection_info::*;
mod connection_info {
    #[derive(Debug, Default)]
    pub struct ConnectionInfo {
        pub ip_address: String,
        pub port: i32,
    }
    impl Default for ConnectionInfo {
        fn default() -> Self {
            Self {
                ip_address: Default::default(),
                port: Default::default(),
            }
        }
    }
    impl rsbinder::Parcelable for ConnectionInfo {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.write(&self.ip_address)?;
            _parcel.write(&self.port)?;
            Ok(())
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            self.ip_address = _parcel.read()?;
            self.port = _parcel.read()?;
            Ok(())
        }
    }
    rsbinder::impl_serialize_for_parcelable!(ConnectionInfo);
    rsbinder::impl_deserialize_for_parcelable!(ConnectionInfo);
    impl rsbinder::ParcelableMetadata for ConnectionInfo {
        fn get_descriptor() -> &'static str { "android.os.ConnectionInfo" }
    }
}
        "##)?;
    Ok(())
}

#[test]
fn test_unions() -> Result<(), Box<dyn Error>> {
    aidl_generator(r##"
        package android.aidl.tests;

        @Backing(type="byte")
        enum ByteEnum {
            // Comment about FOO.
            FOO = 1,
            BAR = 2,
            BAZ,
        }

        @JavaDerive(toString=true, equals=true)
        @RustDerive(Clone=true, PartialEq=true)
        union Union {
            int[] ns = {};
            int n;
            int m;
            @utf8InCpp String s;
            @nullable IBinder ibinder;
            @utf8InCpp List<String> ss;
            ByteEnum be;

            const @utf8InCpp String S1 = "a string constant in union";
        }
        "##,
        r##"
pub use byte_enum::*;
mod byte_enum {
    declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
pub use union::*;
mod union {
    #[derive(Debug, Clone, PartialEq)]
    pub enum Union {
        Ns(Vec<i32>),
        N(i32),
        M(i32),
        S(String),
        Ibinder(Option<rsbinder::StrongIBinder>),
        Ss(Vec<String>),
        Be(crate::aidl::android::aidl::tests::ByteEnum),
    }
    pub const S_1: &str = "a string constant in union";
    impl Default for Union {
        fn default() -> Self {
            Self::Ns(Default::default())
        }
    }
    impl rsbinder::Parcelable for Union {
        fn write_to_parcel(&self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            match self {
                Self::Ns(v) => {
                    parcel.write(&0i32)?;
                    parcel.write(v)
                }
                Self::N(v) => {
                    parcel.write(&1i32)?;
                    parcel.write(v)
                }
                Self::M(v) => {
                    parcel.write(&2i32)?;
                    parcel.write(v)
                }
                Self::S(v) => {
                    parcel.write(&3i32)?;
                    parcel.write(v)
                }
                Self::Ibinder(v) => {
                    parcel.write(&4i32)?;
                    parcel.write(v)
                }
                Self::Ss(v) => {
                    parcel.write(&5i32)?;
                    parcel.write(v)
                }
                Self::Be(v) => {
                    parcel.write(&6i32)?;
                    parcel.write(v)
                }
            }
        }
        fn read_from_parcel(&mut self, parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            let tag: i32 = parcel.read()?;
            match tag {
                0 => {
                    let value: Vec<i32> = parcel.read()?;
                    *self = Self::Ns(value);
                    Ok(())
                }
                1 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::N(value);
                    Ok(())
                }
                2 => {
                    let value: i32 = parcel.read()?;
                    *self = Self::M(value);
                    Ok(())
                }
                3 => {
                    let value: String = parcel.read()?;
                    *self = Self::S(value);
                    Ok(())
                }
                4 => {
                    let value: Option<rsbinder::StrongIBinder> = parcel.read()?;
                    *self = Self::Ibinder(value);
                    Ok(())
                }
                5 => {
                    let value: Vec<String> = parcel.read()?;
                    *self = Self::Ss(value);
                    Ok(())
                }
                6 => {
                    let value: crate::aidl::android::aidl::tests::ByteEnum = parcel.read()?;
                    *self = Self::Be(value);
                    Ok(())
                }
                _ => Err(rsbinder::StatusCode::BadValue.into()),
            }
        }
    }
    rsbinder::impl_serialize_for_parcelable!(Union);
    rsbinder::impl_deserialize_for_parcelable!(Union);
    impl rsbinder::ParcelableMetadata for Union {
        fn get_descriptor() -> &'static str { "android.aidl.tests.Union" }
    }
    pub mod tag {
        rsbinder::declare_binder_enum! {
            Tag : [i32; 7] {
                NS = 0,
                N = 1,
                M = 2,
                S = 3,
                IBINDER = 4,
                SS = 5,
                BE = 6,
            }
        }
    }
}
        "##)?;

    Ok(())
}

#[cfg(test)]
const CONSTANT_EXPRESSION_ENUM: &str = r##"
        @Backing(type="int")
        enum ConstantExpressionEnum {
            // Should be all true / ones.
            // dec literals are either int or long
            decInt32_1 = (~(-1)) == 0,
            decInt32_2 = ~~(1 << 31) == (1 << 31),
            decInt64_1 = (~(-1L)) == 0,
            decInt64_2 = (~4294967295L) != 0,
            decInt64_3 = (~4294967295) != 0,
            decInt64_4 = ~~(1L << 63) == (1L << 63),

            // hex literals could be int or long
            // 0x7fffffff is int, hence can be negated
            hexInt32_1 = -0x7fffffff < 0,

            // 0x80000000 is int32_t max + 1
            hexInt32_2 = 0x80000000 < 0,

            // 0xFFFFFFFF is int32_t, not long; if it were long then ~(long)0xFFFFFFFF != 0
            hexInt32_3 = ~0xFFFFFFFF == 0,

            // 0x7FFFFFFFFFFFFFFF is long, hence can be negated
            hexInt64_1 = -0x7FFFFFFFFFFFFFFF < 0
        }
"##;

#[cfg(test)]
const INT_ENUM: &str = r##"
        @Backing(type="int")
        enum IntEnum {
            FOO = 1000,
            BAR = 2000,
            BAZ,
            /** @deprecated do not use this */
            QUX,
        }
"##;

#[cfg(test)]
const LONG_ENUM: &str =r##"
        @Backing(type="long")
        enum LongEnum {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ,
        }
"##;

#[cfg(test)]
const BYTE_ENUM: &str = r##"
        @Backing(type="byte")
        enum ByteEnum {
            // Comment about FOO.
            FOO = 1,
            BAR = 2,
            BAZ,
        }
"##;

#[test]
fn test_enums() -> Result<(), Box<dyn Error>> {
    aidl_generator(BYTE_ENUM,
        r##"
pub use byte_enum::*;
mod byte_enum {
    declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
        "##)?;

    aidl_generator(r##"
        enum BackendType {
            CPP,
            JAVA,
            NDK,
            RUST,
        }
        "##,
        r##"
pub use backend_type::*;
mod backend_type {
    declare_binder_enum! {
        BackendType : [i8; 4] {
            CPP = 0,
            JAVA = 1,
            NDK = 2,
            RUST = 3,
        }
    }
}
        "##)?;

    aidl_generator(CONSTANT_EXPRESSION_ENUM,
        r##"
pub use constant_expression_enum::*;
mod constant_expression_enum {
    declare_binder_enum! {
        ConstantExpressionEnum : [i32; 10] {
            decInt32_1 = 1,
            decInt32_2 = 1,
            decInt64_1 = 1,
            decInt64_2 = 1,
            decInt64_3 = 1,
            decInt64_4 = 1,
            hexInt32_1 = 1,
            hexInt32_2 = 1,
            hexInt32_3 = 1,
            hexInt64_1 = 1,
        }
    }
}
        "##)?;

    aidl_generator(INT_ENUM,
        r##"
pub use int_enum::*;
mod int_enum {
    declare_binder_enum! {
        IntEnum : [i32; 4] {
            FOO = 1000,
            BAR = 2000,
            BAZ = 2001,
            QUX = 2002,
        }
    }
}
        "##)?;

    aidl_generator(LONG_ENUM,
        r##"
pub use long_enum::*;
mod long_enum {
    declare_binder_enum! {
        LongEnum : [i64; 3] {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ = 200000000001,
        }
    }
}
        "##)?;

    Ok(())
}

#[test]
fn test_byte_parcelable() -> Result<(), Box<dyn Error>> {
    aidl_generator(&(r##"
package android.aidl.tests;
parcelable StructuredParcelable {
    int[] shouldContainThreeFs;
    int f;
    @utf8InCpp String shouldBeJerry;
    ByteEnum shouldBeByteBar;
    IntEnum shouldBeIntBar;
    LongEnum shouldBeLongBar;
    ByteEnum[] shouldContainTwoByteFoos;
    IntEnum[] shouldContainTwoIntFoos;
    LongEnum[] shouldContainTwoLongFoos;

    String stringDefaultsToFoo = "foo";
    byte byteDefaultsToFour = 4;
    int intDefaultsToFive = 5;
    long longDefaultsToNegativeSeven = -7;
    boolean booleanDefaultsToTrue = true;
    char charDefaultsToC = '\'';
    float floatDefaultsToPi = 3.14f;
    double doubleWithDefault = -3.14e17;
    int[] arrayDefaultsTo123 = {
            1,
            2,
            3,
    };
    int[] arrayDefaultsToEmpty = {};

    // Constant expressions that evaluate to 1
    byte[] int8_1 = {
            1,
            0xffu8 + 1 == 0,
            255u8 + 1 == 0,
            0x80u8 == -128,
            // u8 type is reinterpreted as a signed type
            0x80u8 / 2 == -0x40u8,
    };
    int[] int32_1 = {
            (~(-1)) == 0,
            ~~(1 << 31) == (1 << 31),
            -0x7fffffff < 0,
            0x80000000 < 0,

            0x7fffffff == 2147483647,

            // Shifting for more than 31 bits are undefined. Not tested.
            (1 << 31) == 0x80000000,

            // Should be all true / ones.
            (1 + 2) == 3,
            (8 - 9) == -1,
            (9 * 9) == 81,
            (29 / 3) == 9,
            (29 % 3) == 2,
            (0xC0010000 | 0xF00D) == (0xC001F00D),
            (10 | 6) == 14,
            (10 & 6) == 2,
            (10 ^ 6) == 12,
            6 < 10,
            (10 < 10) == 0,
            (6 > 10) == 0,
            (10 > 10) == 0,
            19 >= 10,
            10 >= 10,
            5 <= 10,
            (19 <= 10) == 0,
            19 != 10,
            (10 != 10) == 0,
            (22 << 1) == 44,
            (11 >> 1) == 5,
            (1 || 0) == 1,
            (1 || 1) == 1,
            (0 || 0) == 0,
            (0 || 1) == 1,
            (1 && 0) == 0,
            (1 && 1) == 1,
            (0 && 0) == 0,
            (0 && 1) == 0,

            // precedence tests -- all 1s
            4 == 4,
            -4 < 0,
            0xffffffff == -1,
            4 + 1 == 5,
            2 + 3 - 4,
            2 - 3 + 4 == 3,
            1 == 4 == 0,
            1 && 1,
            1 || 1 && 0, // && higher than ||
            1 < 2,
            !!((3 != 4 || (2 < 3 <= 3 > 4)) >= 0),
            !(1 == 7) && ((3 != 4 || (2 < 3 <= 3 > 4)) >= 0),
            (1 << 2) >= 0,
            (4 >> 1) == 2,
            (8 << -1) == 4,
            (1 << 30 >> 30) == 1,
            (1 | 16 >> 2) == 5,
            (0x0f ^ 0x33 & 0x99) == 0x1e, // & higher than ^
            (~42 & (1 << 3 | 16 >> 2) ^ 7) == 3,
            (2 + 3 - 4 * -7 / (10 % 3)) - 33 == 0,
            (2 + (-3 & 4 / 7)) == 2,
            (((((1 + 0))))),
    };
    long[] int64_1 = {
            (~(-1)) == 0,
            (~4294967295) != 0,
            (~4294967295) != 0,
            ~~(1L << 63) == (1L << 63),
            -0x7FFFFFFFFFFFFFFF < 0,

            0x7fffffff == 2147483647,
            0xfffffffff == 68719476735,
            0xffffffffffffffff == -1,
            (0xfL << 32L) == 0xf00000000,
            (0xfL << 32) == 0xf00000000,
    };
    int hexInt32_pos_1 = -0xffffffff;
    int hexInt64_pos_1 = -0xfffffffffff < 0;

    ConstantExpressionEnum const_exprs_1;
    ConstantExpressionEnum const_exprs_2;
    ConstantExpressionEnum const_exprs_3;
    ConstantExpressionEnum const_exprs_4;
    ConstantExpressionEnum const_exprs_5;
    ConstantExpressionEnum const_exprs_6;
    ConstantExpressionEnum const_exprs_7;
    ConstantExpressionEnum const_exprs_8;
    ConstantExpressionEnum const_exprs_9;
    ConstantExpressionEnum const_exprs_10;

    // String expressions
    @utf8InCpp String addString1 = "hello"
            + " world!";
    @utf8InCpp String addString2 = "The quick brown fox jumps "
            + "over the lazy dog.";

    const int BIT0 = 0x1;
    const int BIT1 = 0x1 << 1;
    const int BIT2 = 0x1 << 2;
    int shouldSetBit0AndBit2;

    @nullable Union u;
    @nullable Union shouldBeConstS1;

    // IntEnum defaultWithFoo = IntEnum.FOO;
}
        "##.to_owned() + CONSTANT_EXPRESSION_ENUM + BYTE_ENUM + INT_ENUM + LONG_ENUM),
        r##"
pub use structured_parcelable::*;
mod structured_parcelable {
    #[derive(Debug, Default)]
    pub struct StructuredParcelable {
        pub int_8_1: Vec<i8>,
        pub int_32_1: Vec<i32>,
        pub int_64_1: Vec<i64>,
        pub hex_int_32_pos_1: i32,
        pub hex_int_64_pos_1: i32,
    }
    impl Default for StructuredParcelable {
        fn default() -> Self {
            Self {
                int_8_1 = vec![1,1,1,1,1,],
                int_32_1 = vec![1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,1,],
                int_64_1 = vec![1,1,1,1,1,1,1,1,1,1,],
                hex_int_32_pos_1 = 1,
                hex_int_64_pos_1 = 1,
            }
        }
    }
    impl rsbinder::Parcelable for StructuredParcelable {
        fn write_to_parcel(&self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            _parcel.write(&self.int_8_1)?;
            _parcel.write(&self.int_32_1)?;
            _parcel.write(&self.int_64_1)?;
            _parcel.write(&self.hex_int_32_pos_1)?;
            _parcel.write(&self.hex_int_64_pos_1)?;
            Ok(())
        }
        fn read_from_parcel(&mut self, _parcel: &mut rsbinder::Parcel) -> rsbinder::Result<()> {
            self.int_8_1 = _parcel.read()?;
            self.int_32_1 = _parcel.read()?;
            self.int_64_1 = _parcel.read()?;
            self.hex_int_32_pos_1 = _parcel.read()?;
            self.hex_int_64_pos_1 = _parcel.read()?;
            Ok(())
        }
    }
    rsbinder::impl_serialize_for_parcelable!(StructuredParcelable);
    rsbinder::impl_deserialize_for_parcelable!(StructuredParcelable);
    impl rsbinder::ParcelableMetadata for StructuredParcelable {
        fn get_descriptor() -> &'static str { "android.aidl.tests.StructuredParcelable" }
    }
}
pub use constant_expression_enum::*;
mod constant_expression_enum {
    declare_binder_enum! {
        ConstantExpressionEnum : [i32; 10] {
            decInt32_1 = 1,
            decInt32_2 = 1,
            decInt64_1 = 1,
            decInt64_2 = 1,
            decInt64_3 = 1,
            decInt64_4 = 1,
            hexInt32_1 = 1,
            hexInt32_2 = 1,
            hexInt32_3 = 1,
            hexInt64_1 = 1,
        }
    }
}
pub use byte_enum::*;
mod byte_enum {
    declare_binder_enum! {
        ByteEnum : [i8; 3] {
            FOO = 1,
            BAR = 2,
            BAZ = 3,
        }
    }
}
pub use int_enum::*;
mod int_enum {
    declare_binder_enum! {
        IntEnum : [i32; 4] {
            FOO = 1000,
            BAR = 2000,
            BAZ = 2001,
            QUX = 2002,
        }
    }
}
pub use long_enum::*;
mod long_enum {
    declare_binder_enum! {
        LongEnum : [i64; 3] {
            FOO = 100000000000,
            BAR = 200000000000,
            BAZ = 200000000001,
        }
    }
}
        "##)?;
    Ok(())
}
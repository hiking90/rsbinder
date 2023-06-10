#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
use binder::declare_binder_interface;
declare_binder_interface! {
  IServiceManager["android.os.IServiceManager"] {
    native: BnServiceManager(on_transact),
    proxy: BpServiceManager {
    },
    async: IServiceManagerAsync,
  }
}
pub trait IServiceManager: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceManager" }
  fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>>;
  fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>>;
  fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()>;
  fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>>;
  fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()>;
  fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()>;
  fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool>;
  fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>>;
  fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>>;
  fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>>;
  fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()>;
  fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()>;
  fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>>;
  fn getDefaultImpl() -> IServiceManagerDefaultRef where Self: Sized {
    DEFAULT_IMPL.lock().unwrap().clone()
  }
  fn setDefaultImpl(d: IServiceManagerDefaultRef) -> IServiceManagerDefaultRef where Self: Sized {
    std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
  }
}
pub trait IServiceManagerAsync<P>: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceManager" }
  fn getService<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<binder::SpIBinder>>>;
  fn checkService<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<binder::SpIBinder>>>;
  fn addService<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::BoxFuture<'a, binder::Result<()>>;
  fn listServices<'a>(&'a self, _arg_dumpPriority: i32) -> binder::BoxFuture<'a, binder::Result<Vec<String>>>;
  fn registerForNotifications<'a>(&'a self, _arg_name: &'a str, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::BoxFuture<'a, binder::Result<()>>;
  fn unregisterForNotifications<'a>(&'a self, _arg_name: &'a str, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::BoxFuture<'a, binder::Result<()>>;
  fn isDeclared<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<bool>>;
  fn getDeclaredInstances<'a>(&'a self, _arg_iface: &'a str) -> binder::BoxFuture<'a, binder::Result<Vec<String>>>;
  fn updatableViaApex<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<String>>>;
  fn getConnectionInfo<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>>>;
  fn registerClientCallback<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::BoxFuture<'a, binder::Result<()>>;
  fn tryUnregisterService<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder) -> binder::BoxFuture<'a, binder::Result<()>>;
  fn getServiceDebugInfo<'a>(&'a self) -> binder::BoxFuture<'a, binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>>>;
}
#[::async_trait::async_trait]
pub trait IServiceManagerAsyncServer: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceManager" }
  async fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>>;
  async fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>>;
  async fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()>;
  async fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>>;
  async fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()>;
  async fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()>;
  async fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool>;
  async fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>>;
  async fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>>;
  async fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>>;
  async fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()>;
  async fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()>;
  async fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>>;
}
impl BnServiceManager {
  /// Create a new async binder service.
  pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IServiceManager>
  where
    T: IServiceManagerAsyncServer + binder::Interface + Send + Sync + 'static,
    R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
  {
    struct Wrapper<T, R> {
      _inner: T,
      _rt: R,
    }
    impl<T, R> binder::Interface for Wrapper<T, R> where T: binder::Interface, R: Send + Sync {
      fn as_binder(&self) -> binder::SpIBinder { self._inner.as_binder() }
      fn dump(&self, _file: &std::fs::File, _args: &[&std::ffi::CStr]) -> std::result::Result<(), binder::StatusCode> { self._inner.dump(_file, _args) }
    }
    impl<T, R> IServiceManager for Wrapper<T, R>
    where
      T: IServiceManagerAsyncServer + Send + Sync + 'static,
      R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
    {
      fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
        self._rt.block_on(self._inner.getService(_arg_name))
      }
      fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
        self._rt.block_on(self._inner.checkService(_arg_name))
      }
      fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()> {
        self._rt.block_on(self._inner.addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority))
      }
      fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>> {
        self._rt.block_on(self._inner.listServices(_arg_dumpPriority))
      }
      fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
        self._rt.block_on(self._inner.registerForNotifications(_arg_name, _arg_callback))
      }
      fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
        self._rt.block_on(self._inner.unregisterForNotifications(_arg_name, _arg_callback))
      }
      fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool> {
        self._rt.block_on(self._inner.isDeclared(_arg_name))
      }
      fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>> {
        self._rt.block_on(self._inner.getDeclaredInstances(_arg_iface))
      }
      fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>> {
        self._rt.block_on(self._inner.updatableViaApex(_arg_name))
      }
      fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>> {
        self._rt.block_on(self._inner.getConnectionInfo(_arg_name))
      }
      fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()> {
        self._rt.block_on(self._inner.registerClientCallback(_arg_name, _arg_service, _arg_callback))
      }
      fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()> {
        self._rt.block_on(self._inner.tryUnregisterService(_arg_name, _arg_service))
      }
      fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>> {
        self._rt.block_on(self._inner.getServiceDebugInfo())
      }
    }
    let wrapped = Wrapper { _inner: inner, _rt: rt };
    Self::new_binder(wrapped, features)
  }
}
pub trait IServiceManagerDefault: Send + Sync {
  fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
  fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
}
pub mod transactions {
  pub const getService: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 0;
  pub const checkService: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 1;
  pub const addService: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 2;
  pub const listServices: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 3;
  pub const registerForNotifications: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 4;
  pub const unregisterForNotifications: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 5;
  pub const isDeclared: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 6;
  pub const getDeclaredInstances: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 7;
  pub const updatableViaApex: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 8;
  pub const getConnectionInfo: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 9;
  pub const registerClientCallback: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 10;
  pub const tryUnregisterService: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 11;
  pub const getServiceDebugInfo: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 12;
}
pub type IServiceManagerDefaultRef = Option<std::sync::Arc<dyn IServiceManagerDefault>>;
use lazy_static::lazy_static;
lazy_static! {
  static ref DEFAULT_IMPL: std::sync::Mutex<IServiceManagerDefaultRef> = std::sync::Mutex::new(None);
}
pub const DUMP_FLAG_PRIORITY_CRITICAL: i32 = 1;
pub const DUMP_FLAG_PRIORITY_HIGH: i32 = 2;
pub const DUMP_FLAG_PRIORITY_NORMAL: i32 = 4;
pub const DUMP_FLAG_PRIORITY_DEFAULT: i32 = 8;
pub const DUMP_FLAG_PRIORITY_ALL: i32 = 15;
pub const DUMP_FLAG_PROTO: i32 = 16;
impl BpServiceManager {
  fn build_parcel_getService(&self, _arg_name: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    Ok(aidl_data)
  }
  fn read_response_getService(&self, _arg_name: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Option<binder::SpIBinder>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.getService(_arg_name);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Option<binder::SpIBinder> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_checkService(&self, _arg_name: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    Ok(aidl_data)
  }
  fn read_response_checkService(&self, _arg_name: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Option<binder::SpIBinder>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.checkService(_arg_name);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Option<binder::SpIBinder> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_service)?;
    aidl_data.write(&_arg_allowIsolated)?;
    aidl_data.write(&_arg_dumpPriority)?;
    Ok(aidl_data)
  }
  fn read_response_addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    Ok(())
  }
  fn build_parcel_listServices(&self, _arg_dumpPriority: i32) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(&_arg_dumpPriority)?;
    Ok(aidl_data)
  }
  fn read_response_listServices(&self, _arg_dumpPriority: i32, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Vec<String>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.listServices(_arg_dumpPriority);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Vec<String> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_callback)?;
    Ok(aidl_data)
  }
  fn read_response_registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.registerForNotifications(_arg_name, _arg_callback);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    Ok(())
  }
  fn build_parcel_unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_callback)?;
    Ok(aidl_data)
  }
  fn read_response_unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.unregisterForNotifications(_arg_name, _arg_callback);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    Ok(())
  }
  fn build_parcel_isDeclared(&self, _arg_name: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    Ok(aidl_data)
  }
  fn read_response_isDeclared(&self, _arg_name: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<bool> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.isDeclared(_arg_name);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: bool = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_iface)?;
    Ok(aidl_data)
  }
  fn read_response_getDeclaredInstances(&self, _arg_iface: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Vec<String>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.getDeclaredInstances(_arg_iface);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Vec<String> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_updatableViaApex(&self, _arg_name: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    Ok(aidl_data)
  }
  fn read_response_updatableViaApex(&self, _arg_name: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Option<String>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.updatableViaApex(_arg_name);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Option<String> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_getConnectionInfo(&self, _arg_name: &str) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    Ok(aidl_data)
  }
  fn read_response_getConnectionInfo(&self, _arg_name: &str, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.getConnectionInfo(_arg_name);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Option<crate::mangled::_7_android_2_os_14_ConnectionInfo> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
  fn build_parcel_registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_service)?;
    aidl_data.write(_arg_callback)?;
    Ok(aidl_data)
  }
  fn read_response_registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.registerClientCallback(_arg_name, _arg_service, _arg_callback);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    Ok(())
  }
  fn build_parcel_tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_service)?;
    Ok(aidl_data)
  }
  fn read_response_tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.tryUnregisterService(_arg_name, _arg_service);
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    Ok(())
  }
  fn build_parcel_getServiceDebugInfo(&self) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    Ok(aidl_data)
  }
  fn read_response_getServiceDebugInfo(&self, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceManager>::getDefaultImpl() {
        return _aidl_default_impl.getServiceDebugInfo();
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo> = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
}
impl IServiceManager for BpServiceManager {
  fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
    let _aidl_data = self.build_parcel_getService(_arg_name)?;
    let _aidl_reply = self.binder.submit_transact(transactions::getService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_getService(_arg_name, _aidl_reply)
  }
  fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> {
    let _aidl_data = self.build_parcel_checkService(_arg_name)?;
    let _aidl_reply = self.binder.submit_transact(transactions::checkService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_checkService(_arg_name, _aidl_reply)
  }
  fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority)?;
    let _aidl_reply = self.binder.submit_transact(transactions::addService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority, _aidl_reply)
  }
  fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>> {
    let _aidl_data = self.build_parcel_listServices(_arg_dumpPriority)?;
    let _aidl_reply = self.binder.submit_transact(transactions::listServices, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_listServices(_arg_dumpPriority, _aidl_reply)
  }
  fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_registerForNotifications(_arg_name, _arg_callback)?;
    let _aidl_reply = self.binder.submit_transact(transactions::registerForNotifications, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_registerForNotifications(_arg_name, _arg_callback, _aidl_reply)
  }
  fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_unregisterForNotifications(_arg_name, _arg_callback)?;
    let _aidl_reply = self.binder.submit_transact(transactions::unregisterForNotifications, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_unregisterForNotifications(_arg_name, _arg_callback, _aidl_reply)
  }
  fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool> {
    let _aidl_data = self.build_parcel_isDeclared(_arg_name)?;
    let _aidl_reply = self.binder.submit_transact(transactions::isDeclared, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_isDeclared(_arg_name, _aidl_reply)
  }
  fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>> {
    let _aidl_data = self.build_parcel_getDeclaredInstances(_arg_iface)?;
    let _aidl_reply = self.binder.submit_transact(transactions::getDeclaredInstances, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_getDeclaredInstances(_arg_iface, _aidl_reply)
  }
  fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>> {
    let _aidl_data = self.build_parcel_updatableViaApex(_arg_name)?;
    let _aidl_reply = self.binder.submit_transact(transactions::updatableViaApex, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_updatableViaApex(_arg_name, _aidl_reply)
  }
  fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>> {
    let _aidl_data = self.build_parcel_getConnectionInfo(_arg_name)?;
    let _aidl_reply = self.binder.submit_transact(transactions::getConnectionInfo, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_getConnectionInfo(_arg_name, _aidl_reply)
  }
  fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_registerClientCallback(_arg_name, _arg_service, _arg_callback)?;
    let _aidl_reply = self.binder.submit_transact(transactions::registerClientCallback, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_registerClientCallback(_arg_name, _arg_service, _arg_callback, _aidl_reply)
  }
  fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_tryUnregisterService(_arg_name, _arg_service)?;
    let _aidl_reply = self.binder.submit_transact(transactions::tryUnregisterService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_tryUnregisterService(_arg_name, _arg_service, _aidl_reply)
  }
  fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>> {
    let _aidl_data = self.build_parcel_getServiceDebugInfo()?;
    let _aidl_reply = self.binder.submit_transact(transactions::getServiceDebugInfo, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_getServiceDebugInfo(_aidl_reply)
  }
}
impl<P: binder::BinderAsyncPool> IServiceManagerAsync<P> for BpServiceManager {
  fn getService<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<binder::SpIBinder>>> {
    let _aidl_data = match self.build_parcel_getService(_arg_name) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::getService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_getService(_arg_name, _aidl_reply)
      }
    )
  }
  fn checkService<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<binder::SpIBinder>>> {
    let _aidl_data = match self.build_parcel_checkService(_arg_name) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::checkService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_checkService(_arg_name, _aidl_reply)
      }
    )
  }
  fn addService<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::BoxFuture<'a, binder::Result<()>> {
    let _aidl_data = match self.build_parcel_addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::addService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority, _aidl_reply)
      }
    )
  }
  fn listServices<'a>(&'a self, _arg_dumpPriority: i32) -> binder::BoxFuture<'a, binder::Result<Vec<String>>> {
    let _aidl_data = match self.build_parcel_listServices(_arg_dumpPriority) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::listServices, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_listServices(_arg_dumpPriority, _aidl_reply)
      }
    )
  }
  fn registerForNotifications<'a>(&'a self, _arg_name: &'a str, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::BoxFuture<'a, binder::Result<()>> {
    let _aidl_data = match self.build_parcel_registerForNotifications(_arg_name, _arg_callback) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::registerForNotifications, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_registerForNotifications(_arg_name, _arg_callback, _aidl_reply)
      }
    )
  }
  fn unregisterForNotifications<'a>(&'a self, _arg_name: &'a str, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::BoxFuture<'a, binder::Result<()>> {
    let _aidl_data = match self.build_parcel_unregisterForNotifications(_arg_name, _arg_callback) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::unregisterForNotifications, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_unregisterForNotifications(_arg_name, _arg_callback, _aidl_reply)
      }
    )
  }
  fn isDeclared<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<bool>> {
    let _aidl_data = match self.build_parcel_isDeclared(_arg_name) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::isDeclared, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_isDeclared(_arg_name, _aidl_reply)
      }
    )
  }
  fn getDeclaredInstances<'a>(&'a self, _arg_iface: &'a str) -> binder::BoxFuture<'a, binder::Result<Vec<String>>> {
    let _aidl_data = match self.build_parcel_getDeclaredInstances(_arg_iface) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::getDeclaredInstances, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_getDeclaredInstances(_arg_iface, _aidl_reply)
      }
    )
  }
  fn updatableViaApex<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<String>>> {
    let _aidl_data = match self.build_parcel_updatableViaApex(_arg_name) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::updatableViaApex, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_updatableViaApex(_arg_name, _aidl_reply)
      }
    )
  }
  fn getConnectionInfo<'a>(&'a self, _arg_name: &'a str) -> binder::BoxFuture<'a, binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>>> {
    let _aidl_data = match self.build_parcel_getConnectionInfo(_arg_name) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::getConnectionInfo, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_getConnectionInfo(_arg_name, _aidl_reply)
      }
    )
  }
  fn registerClientCallback<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder, _arg_callback: &'a binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::BoxFuture<'a, binder::Result<()>> {
    let _aidl_data = match self.build_parcel_registerClientCallback(_arg_name, _arg_service, _arg_callback) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::registerClientCallback, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_registerClientCallback(_arg_name, _arg_service, _arg_callback, _aidl_reply)
      }
    )
  }
  fn tryUnregisterService<'a>(&'a self, _arg_name: &'a str, _arg_service: &'a binder::SpIBinder) -> binder::BoxFuture<'a, binder::Result<()>> {
    let _aidl_data = match self.build_parcel_tryUnregisterService(_arg_name, _arg_service) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::tryUnregisterService, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_tryUnregisterService(_arg_name, _arg_service, _aidl_reply)
      }
    )
  }
  fn getServiceDebugInfo<'a>(&'a self) -> binder::BoxFuture<'a, binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>>> {
    let _aidl_data = match self.build_parcel_getServiceDebugInfo() {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::getServiceDebugInfo, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_getServiceDebugInfo(_aidl_reply)
      }
    )
  }
}
impl IServiceManager for binder::binder_impl::Binder<BnServiceManager> {
  fn getService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> { self.0.getService(_arg_name) }
  fn checkService(&self, _arg_name: &str) -> binder::Result<Option<binder::SpIBinder>> { self.0.checkService(_arg_name) }
  fn addService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_allowIsolated: bool, _arg_dumpPriority: i32) -> binder::Result<()> { self.0.addService(_arg_name, _arg_service, _arg_allowIsolated, _arg_dumpPriority) }
  fn listServices(&self, _arg_dumpPriority: i32) -> binder::Result<Vec<String>> { self.0.listServices(_arg_dumpPriority) }
  fn registerForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> { self.0.registerForNotifications(_arg_name, _arg_callback) }
  fn unregisterForNotifications(&self, _arg_name: &str, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback>) -> binder::Result<()> { self.0.unregisterForNotifications(_arg_name, _arg_callback) }
  fn isDeclared(&self, _arg_name: &str) -> binder::Result<bool> { self.0.isDeclared(_arg_name) }
  fn getDeclaredInstances(&self, _arg_iface: &str) -> binder::Result<Vec<String>> { self.0.getDeclaredInstances(_arg_iface) }
  fn updatableViaApex(&self, _arg_name: &str) -> binder::Result<Option<String>> { self.0.updatableViaApex(_arg_name) }
  fn getConnectionInfo(&self, _arg_name: &str) -> binder::Result<Option<crate::mangled::_7_android_2_os_14_ConnectionInfo>> { self.0.getConnectionInfo(_arg_name) }
  fn registerClientCallback(&self, _arg_name: &str, _arg_service: &binder::SpIBinder, _arg_callback: &binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback>) -> binder::Result<()> { self.0.registerClientCallback(_arg_name, _arg_service, _arg_callback) }
  fn tryUnregisterService(&self, _arg_name: &str, _arg_service: &binder::SpIBinder) -> binder::Result<()> { self.0.tryUnregisterService(_arg_name, _arg_service) }
  fn getServiceDebugInfo(&self) -> binder::Result<Vec<crate::mangled::_7_android_2_os_16_ServiceDebugInfo>> { self.0.getServiceDebugInfo() }
}
fn on_transact(_aidl_service: &dyn IServiceManager, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
  match _aidl_code {
    transactions::getService => {
      let _arg_name: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.getService(&_arg_name);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::checkService => {
      let _arg_name: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.checkService(&_arg_name);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::addService => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_service: binder::SpIBinder = _aidl_data.read()?;
      let _arg_allowIsolated: bool = _aidl_data.read()?;
      let _arg_dumpPriority: i32 = _aidl_data.read()?;
      let _aidl_return = _aidl_service.addService(&_arg_name, &_arg_service, _arg_allowIsolated, _arg_dumpPriority);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::listServices => {
      let _arg_dumpPriority: i32 = _aidl_data.read()?;
      let _aidl_return = _aidl_service.listServices(_arg_dumpPriority);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::registerForNotifications => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_callback: binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback> = _aidl_data.read()?;
      let _aidl_return = _aidl_service.registerForNotifications(&_arg_name, &_arg_callback);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::unregisterForNotifications => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_callback: binder::Strong<dyn crate::mangled::_7_android_2_os_16_IServiceCallback> = _aidl_data.read()?;
      let _aidl_return = _aidl_service.unregisterForNotifications(&_arg_name, &_arg_callback);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::isDeclared => {
      let _arg_name: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.isDeclared(&_arg_name);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::getDeclaredInstances => {
      let _arg_iface: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.getDeclaredInstances(&_arg_iface);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::updatableViaApex => {
      let _arg_name: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.updatableViaApex(&_arg_name);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::getConnectionInfo => {
      let _arg_name: String = _aidl_data.read()?;
      let _aidl_return = _aidl_service.getConnectionInfo(&_arg_name);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::registerClientCallback => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_service: binder::SpIBinder = _aidl_data.read()?;
      let _arg_callback: binder::Strong<dyn crate::mangled::_7_android_2_os_15_IClientCallback> = _aidl_data.read()?;
      let _aidl_return = _aidl_service.registerClientCallback(&_arg_name, &_arg_service, &_arg_callback);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::tryUnregisterService => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_service: binder::SpIBinder = _aidl_data.read()?;
      let _aidl_return = _aidl_service.tryUnregisterService(&_arg_name, &_arg_service);
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    transactions::getServiceDebugInfo => {
      let _aidl_return = _aidl_service.getServiceDebugInfo();
      match &_aidl_return {
        Ok(_aidl_return) => {
          _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
          _aidl_reply.write(_aidl_return)?;
        }
        Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
      }
      Ok(())
    }
    _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
  }
}
pub(crate) mod mangled {
 pub use super::IServiceManager as _7_android_2_os_15_IServiceManager;
}

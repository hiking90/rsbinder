#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
use binder::declare_binder_interface;
declare_binder_interface! {
  IClientCallback["android.os.IClientCallback"] {
    native: BnClientCallback(on_transact),
    proxy: BpClientCallback {
    },
    async: IClientCallbackAsync,
  }
}
pub trait IClientCallback: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IClientCallback" }
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()>;
  fn getDefaultImpl() -> IClientCallbackDefaultRef where Self: Sized {
    DEFAULT_IMPL.lock().unwrap().clone()
  }
  fn setDefaultImpl(d: IClientCallbackDefaultRef) -> IClientCallbackDefaultRef where Self: Sized {
    std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
  }
}
pub trait IClientCallbackAsync<P>: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IClientCallback" }
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> std::future::Ready<binder::Result<()>>;
}
#[::async_trait::async_trait]
pub trait IClientCallbackAsyncServer: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IClientCallback" }
  async fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()>;
}
impl BnClientCallback {
  /// Create a new async binder service.
  pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IClientCallback>
  where
    T: IClientCallbackAsyncServer + binder::Interface + Send + Sync + 'static,
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
    impl<T, R> IClientCallback for Wrapper<T, R>
    where
      T: IClientCallbackAsyncServer + Send + Sync + 'static,
      R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
    {
      fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()> {
        self._rt.block_on(self._inner.onClients(_arg_registered, _arg_hasClients))
      }
    }
    let wrapped = Wrapper { _inner: inner, _rt: rt };
    Self::new_binder(wrapped, features)
  }
}
pub trait IClientCallbackDefault: Send + Sync {
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
}
pub mod transactions {
  pub const onClients: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 0;
}
pub type IClientCallbackDefaultRef = Option<std::sync::Arc<dyn IClientCallbackDefault>>;
use lazy_static::lazy_static;
lazy_static! {
  static ref DEFAULT_IMPL: std::sync::Mutex<IClientCallbackDefaultRef> = std::sync::Mutex::new(None);
}
impl BpClientCallback {
  fn build_parcel_onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_registered)?;
    aidl_data.write(&_arg_hasClients)?;
    Ok(aidl_data)
  }
  fn read_response_onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IClientCallback>::getDefaultImpl() {
        return _aidl_default_impl.onClients(_arg_registered, _arg_hasClients);
      }
    }
    let _aidl_reply = _aidl_reply?;
    Ok(())
  }
}
impl IClientCallback for BpClientCallback {
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_onClients(_arg_registered, _arg_hasClients)?;
    let _aidl_reply = self.binder.submit_transact(transactions::onClients, _aidl_data, binder::binder_impl::FLAG_ONEWAY | binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_onClients(_arg_registered, _arg_hasClients, _aidl_reply)
  }
}
impl<P: binder::BinderAsyncPool> IClientCallbackAsync<P> for BpClientCallback {
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> std::future::Ready<binder::Result<()>> {
    let _aidl_data = match self.build_parcel_onClients(_arg_registered, _arg_hasClients) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return std::future::ready(Err(err)),
    };
    let _aidl_reply = self.binder.submit_transact(transactions::onClients, _aidl_data, binder::binder_impl::FLAG_ONEWAY | binder::binder_impl::FLAG_PRIVATE_LOCAL);
    std::future::ready(self.read_response_onClients(_arg_registered, _arg_hasClients, _aidl_reply))
  }
}
impl IClientCallback for binder::binder_impl::Binder<BnClientCallback> {
  fn onClients(&self, _arg_registered: &binder::SpIBinder, _arg_hasClients: bool) -> binder::Result<()> { self.0.onClients(_arg_registered, _arg_hasClients) }
}
fn on_transact(_aidl_service: &dyn IClientCallback, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
  match _aidl_code {
    transactions::onClients => {
      let _arg_registered: binder::SpIBinder = _aidl_data.read()?;
      let _arg_hasClients: bool = _aidl_data.read()?;
      let _aidl_return = _aidl_service.onClients(&_arg_registered, _arg_hasClients);
      Ok(())
    }
    _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
  }
}
pub(crate) mod mangled {
 pub use super::IClientCallback as _7_android_2_os_15_IClientCallback;
}

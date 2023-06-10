#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
use binder::declare_binder_interface;
declare_binder_interface! {
  IServiceCallback["android.os.IServiceCallback"] {
    native: BnServiceCallback(on_transact),
    proxy: BpServiceCallback {
    },
    async: IServiceCallbackAsync,
  }
}
pub trait IServiceCallback: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceCallback" }
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()>;
  fn getDefaultImpl() -> IServiceCallbackDefaultRef where Self: Sized {
    DEFAULT_IMPL.lock().unwrap().clone()
  }
  fn setDefaultImpl(d: IServiceCallbackDefaultRef) -> IServiceCallbackDefaultRef where Self: Sized {
    std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
  }
}
pub trait IServiceCallbackAsync<P>: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceCallback" }
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> std::future::Ready<binder::Result<()>>;
}
#[::async_trait::async_trait]
pub trait IServiceCallbackAsyncServer: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.os.IServiceCallback" }
  async fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()>;
}
impl BnServiceCallback {
  /// Create a new async binder service.
  pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IServiceCallback>
  where
    T: IServiceCallbackAsyncServer + binder::Interface + Send + Sync + 'static,
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
    impl<T, R> IServiceCallback for Wrapper<T, R>
    where
      T: IServiceCallbackAsyncServer + Send + Sync + 'static,
      R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
    {
      fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()> {
        self._rt.block_on(self._inner.onRegistration(_arg_name, _arg_binder))
      }
    }
    let wrapped = Wrapper { _inner: inner, _rt: rt };
    Self::new_binder(wrapped, features)
  }
}
pub trait IServiceCallbackDefault: Send + Sync {
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
}
pub mod transactions {
  pub const onRegistration: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 0;
}
pub type IServiceCallbackDefaultRef = Option<std::sync::Arc<dyn IServiceCallbackDefault>>;
use lazy_static::lazy_static;
lazy_static! {
  static ref DEFAULT_IMPL: std::sync::Mutex<IServiceCallbackDefaultRef> = std::sync::Mutex::new(None);
}
impl BpServiceCallback {
  fn build_parcel_onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    aidl_data.write(_arg_name)?;
    aidl_data.write(_arg_binder)?;
    Ok(aidl_data)
  }
  fn read_response_onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<()> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as IServiceCallback>::getDefaultImpl() {
        return _aidl_default_impl.onRegistration(_arg_name, _arg_binder);
      }
    }
    let _aidl_reply = _aidl_reply?;
    Ok(())
  }
}
impl IServiceCallback for BpServiceCallback {
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()> {
    let _aidl_data = self.build_parcel_onRegistration(_arg_name, _arg_binder)?;
    let _aidl_reply = self.binder.submit_transact(transactions::onRegistration, _aidl_data, binder::binder_impl::FLAG_ONEWAY | binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_onRegistration(_arg_name, _arg_binder, _aidl_reply)
  }
}
impl<P: binder::BinderAsyncPool> IServiceCallbackAsync<P> for BpServiceCallback {
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> std::future::Ready<binder::Result<()>> {
    let _aidl_data = match self.build_parcel_onRegistration(_arg_name, _arg_binder) {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return std::future::ready(Err(err)),
    };
    let _aidl_reply = self.binder.submit_transact(transactions::onRegistration, _aidl_data, binder::binder_impl::FLAG_ONEWAY | binder::binder_impl::FLAG_PRIVATE_LOCAL);
    std::future::ready(self.read_response_onRegistration(_arg_name, _arg_binder, _aidl_reply))
  }
}
impl IServiceCallback for binder::binder_impl::Binder<BnServiceCallback> {
  fn onRegistration(&self, _arg_name: &str, _arg_binder: &binder::SpIBinder) -> binder::Result<()> { self.0.onRegistration(_arg_name, _arg_binder) }
}
fn on_transact(_aidl_service: &dyn IServiceCallback, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
  match _aidl_code {
    transactions::onRegistration => {
      let _arg_name: String = _aidl_data.read()?;
      let _arg_binder: binder::SpIBinder = _aidl_data.read()?;
      let _aidl_return = _aidl_service.onRegistration(&_arg_name, &_arg_binder);
      Ok(())
    }
    _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
  }
}
pub(crate) mod mangled {
 pub use super::IServiceCallback as _7_android_2_os_16_IServiceCallback;
}

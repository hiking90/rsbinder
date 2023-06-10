#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
use binder::declare_binder_interface;
declare_binder_interface! {
  INamedCallback["android.aidl.tests.INamedCallback"] {
    native: BnNamedCallback(on_transact),
    proxy: BpNamedCallback {
    },
    async: INamedCallbackAsync,
  }
}
pub trait INamedCallback: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.INamedCallback" }
  fn GetName(&self) -> binder::Result<String>;
  fn getDefaultImpl() -> INamedCallbackDefaultRef where Self: Sized {
    DEFAULT_IMPL.lock().unwrap().clone()
  }
  fn setDefaultImpl(d: INamedCallbackDefaultRef) -> INamedCallbackDefaultRef where Self: Sized {
    std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
  }
}
pub trait INamedCallbackAsync<P>: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.INamedCallback" }
  fn GetName<'a>(&'a self) -> binder::BoxFuture<'a, binder::Result<String>>;
}
#[::async_trait::async_trait]
pub trait INamedCallbackAsyncServer: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.INamedCallback" }
  async fn GetName(&self) -> binder::Result<String>;
}
impl BnNamedCallback {
  /// Create a new async binder service.
  pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn INamedCallback>
  where
    T: INamedCallbackAsyncServer + binder::Interface + Send + Sync + 'static,
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
    impl<T, R> INamedCallback for Wrapper<T, R>
    where
      T: INamedCallbackAsyncServer + Send + Sync + 'static,
      R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
    {
      fn GetName(&self) -> binder::Result<String> {
        self._rt.block_on(self._inner.GetName())
      }
    }
    let wrapped = Wrapper { _inner: inner, _rt: rt };
    Self::new_binder(wrapped, features)
  }
}
pub trait INamedCallbackDefault: Send + Sync {
  fn GetName(&self) -> binder::Result<String> {
    Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
  }
}
pub mod transactions {
  pub const GetName: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 0;
}
pub type INamedCallbackDefaultRef = Option<std::sync::Arc<dyn INamedCallbackDefault>>;
use lazy_static::lazy_static;
lazy_static! {
  static ref DEFAULT_IMPL: std::sync::Mutex<INamedCallbackDefaultRef> = std::sync::Mutex::new(None);
}
impl BpNamedCallback {
  fn build_parcel_GetName(&self) -> binder::Result<binder::binder_impl::Parcel> {
    let mut aidl_data = self.binder.prepare_transact()?;
    Ok(aidl_data)
  }
  fn read_response_GetName(&self, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<String> {
    if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
      if let Some(_aidl_default_impl) = <Self as INamedCallback>::getDefaultImpl() {
        return _aidl_default_impl.GetName();
      }
    }
    let _aidl_reply = _aidl_reply?;
    let _aidl_status: binder::Status = _aidl_reply.read()?;
    if !_aidl_status.is_ok() { return Err(_aidl_status); }
    let _aidl_return: String = _aidl_reply.read()?;
    Ok(_aidl_return)
  }
}
impl INamedCallback for BpNamedCallback {
  fn GetName(&self) -> binder::Result<String> {
    let _aidl_data = self.build_parcel_GetName()?;
    let _aidl_reply = self.binder.submit_transact(transactions::GetName, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
    self.read_response_GetName(_aidl_reply)
  }
}
impl<P: binder::BinderAsyncPool> INamedCallbackAsync<P> for BpNamedCallback {
  fn GetName<'a>(&'a self) -> binder::BoxFuture<'a, binder::Result<String>> {
    let _aidl_data = match self.build_parcel_GetName() {
      Ok(_aidl_data) => _aidl_data,
      Err(err) => return Box::pin(std::future::ready(Err(err))),
    };
    let binder = self.binder.clone();
    P::spawn(
      move || binder.submit_transact(transactions::GetName, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
      move |_aidl_reply| async move {
        self.read_response_GetName(_aidl_reply)
      }
    )
  }
}
impl INamedCallback for binder::binder_impl::Binder<BnNamedCallback> {
  fn GetName(&self) -> binder::Result<String> { self.0.GetName() }
}
fn on_transact(_aidl_service: &dyn INamedCallback, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
  match _aidl_code {
    transactions::GetName => {
      let _aidl_return = _aidl_service.GetName();
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
 pub use super::INamedCallback as _7_android_4_aidl_5_tests_14_INamedCallback;
}

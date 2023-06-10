#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
#![allow(non_snake_case)]
#[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
use binder::declare_binder_interface;
declare_binder_interface! {
  IDeprecated["android.aidl.tests.IDeprecated"] {
    native: BnDeprecated(on_transact),
    proxy: BpDeprecated {
    },
    async: IDeprecatedAsync,
  }
}
#[deprecated = "test"]
pub trait IDeprecated: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.IDeprecated" }
  fn getDefaultImpl() -> IDeprecatedDefaultRef where Self: Sized {
    DEFAULT_IMPL.lock().unwrap().clone()
  }
  fn setDefaultImpl(d: IDeprecatedDefaultRef) -> IDeprecatedDefaultRef where Self: Sized {
    std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
  }
}
#[deprecated = "test"]
pub trait IDeprecatedAsync<P>: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.IDeprecated" }
}
#[deprecated = "test"]
#[::async_trait::async_trait]
pub trait IDeprecatedAsyncServer: binder::Interface + Send {
  fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.IDeprecated" }
}
impl BnDeprecated {
  /// Create a new async binder service.
  pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IDeprecated>
  where
    T: IDeprecatedAsyncServer + binder::Interface + Send + Sync + 'static,
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
    impl<T, R> IDeprecated for Wrapper<T, R>
    where
      T: IDeprecatedAsyncServer + Send + Sync + 'static,
      R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
    {
    }
    let wrapped = Wrapper { _inner: inner, _rt: rt };
    Self::new_binder(wrapped, features)
  }
}
pub trait IDeprecatedDefault: Send + Sync {
}
pub mod transactions {
}
pub type IDeprecatedDefaultRef = Option<std::sync::Arc<dyn IDeprecatedDefault>>;
use lazy_static::lazy_static;
lazy_static! {
  static ref DEFAULT_IMPL: std::sync::Mutex<IDeprecatedDefaultRef> = std::sync::Mutex::new(None);
}
impl BpDeprecated {
}
impl IDeprecated for BpDeprecated {
}
impl<P: binder::BinderAsyncPool> IDeprecatedAsync<P> for BpDeprecated {
}
impl IDeprecated for binder::binder_impl::Binder<BnDeprecated> {
}
fn on_transact(_aidl_service: &dyn IDeprecated, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
  match _aidl_code {
    _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
  }
}
pub(crate) mod mangled {
 pub use super::IDeprecated as _7_android_4_aidl_5_tests_11_IDeprecated;
}

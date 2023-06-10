#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct ListOfInterfaces {
}
impl Default for ListOfInterfaces {
  fn default() -> Self {
    Self {
    }
  }
}
impl binder::Parcelable for ListOfInterfaces {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(ListOfInterfaces);
binder::impl_deserialize_for_parcelable!(ListOfInterfaces);
impl binder::binder_impl::ParcelableMetadata for ListOfInterfaces {
  fn get_descriptor() -> &'static str { "android.aidl.tests.ListOfInterfaces" }
}
pub mod IEmptyInterface {
  #![allow(non_upper_case_globals)]
  #![allow(non_snake_case)]
  #[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
  use binder::declare_binder_interface;
  declare_binder_interface! {
    IEmptyInterface["android.aidl.tests.ListOfInterfaces.IEmptyInterface"] {
      native: BnEmptyInterface(on_transact),
      proxy: BpEmptyInterface {
      },
      async: IEmptyInterfaceAsync,
    }
  }
  pub trait IEmptyInterface: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IEmptyInterface" }
    fn getDefaultImpl() -> IEmptyInterfaceDefaultRef where Self: Sized {
      DEFAULT_IMPL.lock().unwrap().clone()
    }
    fn setDefaultImpl(d: IEmptyInterfaceDefaultRef) -> IEmptyInterfaceDefaultRef where Self: Sized {
      std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
    }
  }
  pub trait IEmptyInterfaceAsync<P>: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IEmptyInterface" }
  }
  #[::async_trait::async_trait]
  pub trait IEmptyInterfaceAsyncServer: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IEmptyInterface" }
  }
  impl BnEmptyInterface {
    /// Create a new async binder service.
    pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IEmptyInterface>
    where
      T: IEmptyInterfaceAsyncServer + binder::Interface + Send + Sync + 'static,
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
      impl<T, R> IEmptyInterface for Wrapper<T, R>
      where
        T: IEmptyInterfaceAsyncServer + Send + Sync + 'static,
        R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
      {
      }
      let wrapped = Wrapper { _inner: inner, _rt: rt };
      Self::new_binder(wrapped, features)
    }
  }
  pub trait IEmptyInterfaceDefault: Send + Sync {
  }
  pub mod transactions {
  }
  pub type IEmptyInterfaceDefaultRef = Option<std::sync::Arc<dyn IEmptyInterfaceDefault>>;
  use lazy_static::lazy_static;
  lazy_static! {
    static ref DEFAULT_IMPL: std::sync::Mutex<IEmptyInterfaceDefaultRef> = std::sync::Mutex::new(None);
  }
  impl BpEmptyInterface {
  }
  impl IEmptyInterface for BpEmptyInterface {
  }
  impl<P: binder::BinderAsyncPool> IEmptyInterfaceAsync<P> for BpEmptyInterface {
  }
  impl IEmptyInterface for binder::binder_impl::Binder<BnEmptyInterface> {
  }
  fn on_transact(_aidl_service: &dyn IEmptyInterface, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
    match _aidl_code {
      _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
    }
  }
}
pub mod IMyInterface {
  #![allow(non_upper_case_globals)]
  #![allow(non_snake_case)]
  #[allow(unused_imports)] use binder::binder_impl::IBinderInternal;
  use binder::declare_binder_interface;
  declare_binder_interface! {
    IMyInterface["android.aidl.tests.ListOfInterfaces.IMyInterface"] {
      native: BnMyInterface(on_transact),
      proxy: BpMyInterface {
      },
      async: IMyInterfaceAsync,
    }
  }
  pub trait IMyInterface: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IMyInterface" }
    fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>>;
    fn getDefaultImpl() -> IMyInterfaceDefaultRef where Self: Sized {
      DEFAULT_IMPL.lock().unwrap().clone()
    }
    fn setDefaultImpl(d: IMyInterfaceDefaultRef) -> IMyInterfaceDefaultRef where Self: Sized {
      std::mem::replace(&mut *DEFAULT_IMPL.lock().unwrap(), d)
    }
  }
  pub trait IMyInterfaceAsync<P>: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IMyInterface" }
    fn methodWithInterfaces<'a>(&'a self, _arg_iface: &'a binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&'a binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &'a [binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &'a mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &'a mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&'a [Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &'a mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &'a mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::BoxFuture<'a, binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>>>;
  }
  #[::async_trait::async_trait]
  pub trait IMyInterfaceAsyncServer: binder::Interface + Send {
    fn get_descriptor() -> &'static str where Self: Sized { "android.aidl.tests.ListOfInterfaces.IMyInterface" }
    async fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>>;
  }
  impl BnMyInterface {
    /// Create a new async binder service.
    pub fn new_async_binder<T, R>(inner: T, rt: R, features: binder::BinderFeatures) -> binder::Strong<dyn IMyInterface>
    where
      T: IMyInterfaceAsyncServer + binder::Interface + Send + Sync + 'static,
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
      impl<T, R> IMyInterface for Wrapper<T, R>
      where
        T: IMyInterfaceAsyncServer + Send + Sync + 'static,
        R: binder::binder_impl::BinderAsyncRuntime + Send + Sync + 'static,
      {
        fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>> {
          self._rt.block_on(self._inner.methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout))
        }
      }
      let wrapped = Wrapper { _inner: inner, _rt: rt };
      Self::new_binder(wrapped, features)
    }
  }
  pub trait IMyInterfaceDefault: Send + Sync {
    fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>> {
      Err(binder::StatusCode::UNKNOWN_TRANSACTION.into())
    }
  }
  pub mod transactions {
    pub const methodWithInterfaces: binder::binder_impl::TransactionCode = binder::binder_impl::FIRST_CALL_TRANSACTION + 0;
  }
  pub type IMyInterfaceDefaultRef = Option<std::sync::Arc<dyn IMyInterfaceDefault>>;
  use lazy_static::lazy_static;
  lazy_static! {
    static ref DEFAULT_IMPL: std::sync::Mutex<IMyInterfaceDefaultRef> = std::sync::Mutex::new(None);
  }
  impl BpMyInterface {
    fn build_parcel_methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<binder::binder_impl::Parcel> {
      let mut aidl_data = self.binder.prepare_transact()?;
      aidl_data.write(_arg_iface)?;
      aidl_data.write(&_arg_nullable_iface)?;
      aidl_data.write(_arg_iface_list_in)?;
      aidl_data.write(_arg_iface_list_inout)?;
      aidl_data.write(&_arg_nullable_iface_list_in)?;
      aidl_data.write(_arg_nullable_iface_list_inout)?;
      Ok(aidl_data)
    }
    fn read_response_methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _aidl_reply: std::result::Result<binder::binder_impl::Parcel, binder::StatusCode>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>> {
      if let Err(binder::StatusCode::UNKNOWN_TRANSACTION) = _aidl_reply {
        if let Some(_aidl_default_impl) = <Self as IMyInterface>::getDefaultImpl() {
          return _aidl_default_impl.methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout);
        }
      }
      let _aidl_reply = _aidl_reply?;
      let _aidl_status: binder::Status = _aidl_reply.read()?;
      if !_aidl_status.is_ok() { return Err(_aidl_status); }
      let _aidl_return: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>> = _aidl_reply.read()?;
      _aidl_reply.read_onto(_arg_iface_list_out)?;
      _aidl_reply.read_onto(_arg_iface_list_inout)?;
      _aidl_reply.read_onto(_arg_nullable_iface_list_out)?;
      _aidl_reply.read_onto(_arg_nullable_iface_list_inout)?;
      Ok(_aidl_return)
    }
  }
  impl IMyInterface for BpMyInterface {
    fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>> {
      let _aidl_data = self.build_parcel_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout)?;
      let _aidl_reply = self.binder.submit_transact(transactions::methodWithInterfaces, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL);
      self.read_response_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout, _aidl_reply)
    }
  }
  impl<P: binder::BinderAsyncPool> IMyInterfaceAsync<P> for BpMyInterface {
    fn methodWithInterfaces<'a>(&'a self, _arg_iface: &'a binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&'a binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &'a [binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &'a mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &'a mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&'a [Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &'a mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &'a mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::BoxFuture<'a, binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>>> {
      let _aidl_data = match self.build_parcel_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout) {
        Ok(_aidl_data) => _aidl_data,
        Err(err) => return Box::pin(std::future::ready(Err(err))),
      };
      let binder = self.binder.clone();
      P::spawn(
        move || binder.submit_transact(transactions::methodWithInterfaces, _aidl_data, binder::binder_impl::FLAG_PRIVATE_LOCAL),
        move |_aidl_reply| async move {
          self.read_response_methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout, _aidl_reply)
        }
      )
    }
  }
  impl IMyInterface for binder::binder_impl::Binder<BnMyInterface> {
    fn methodWithInterfaces(&self, _arg_iface: &binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>, _arg_nullable_iface: Option<&binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_iface_list_in: &[binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>], _arg_iface_list_out: &mut Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>, _arg_iface_list_inout: &mut Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>, _arg_nullable_iface_list_in: Option<&[Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>]>, _arg_nullable_iface_list_out: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>, _arg_nullable_iface_list_inout: &mut Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>) -> binder::Result<Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>> { self.0.methodWithInterfaces(_arg_iface, _arg_nullable_iface, _arg_iface_list_in, _arg_iface_list_out, _arg_iface_list_inout, _arg_nullable_iface_list_in, _arg_nullable_iface_list_out, _arg_nullable_iface_list_inout) }
  }
  fn on_transact(_aidl_service: &dyn IMyInterface, _aidl_code: binder::binder_impl::TransactionCode, _aidl_data: &binder::binder_impl::BorrowedParcel<'_>, _aidl_reply: &mut binder::binder_impl::BorrowedParcel<'_>) -> std::result::Result<(), binder::StatusCode> {
    match _aidl_code {
      transactions::methodWithInterfaces => {
        let _arg_iface: binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface> = _aidl_data.read()?;
        let _arg_nullable_iface: Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = _aidl_data.read()?;
        let _arg_iface_list_in: Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = _aidl_data.read()?;
        let mut _arg_iface_list_out: Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>> = Default::default();
        let mut _arg_iface_list_inout: Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = _aidl_data.read()?;
        let _arg_nullable_iface_list_in: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>> = _aidl_data.read()?;
        let mut _arg_nullable_iface_list_out: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>> = Default::default();
        let mut _arg_nullable_iface_list_inout: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>> = _aidl_data.read()?;
        let _aidl_return = _aidl_service.methodWithInterfaces(&_arg_iface, _arg_nullable_iface.as_ref(), &_arg_iface_list_in, &mut _arg_iface_list_out, &mut _arg_iface_list_inout, _arg_nullable_iface_list_in.as_deref(), &mut _arg_nullable_iface_list_out, &mut _arg_nullable_iface_list_inout);
        match &_aidl_return {
          Ok(_aidl_return) => {
            _aidl_reply.write(&binder::Status::from(binder::StatusCode::OK))?;
            _aidl_reply.write(_aidl_return)?;
            _aidl_reply.write(&_arg_iface_list_out)?;
            _aidl_reply.write(&_arg_iface_list_inout)?;
            _aidl_reply.write(&_arg_nullable_iface_list_out)?;
            _aidl_reply.write(&_arg_nullable_iface_list_inout)?;
          }
          Err(_aidl_status) => _aidl_reply.write(_aidl_status)?
        }
        Ok(())
      }
      _ => Err(binder::StatusCode::UNKNOWN_TRANSACTION)
    }
  }
}
pub mod MyParcelable {
  #[derive(Debug)]
  pub struct MyParcelable {
    pub iface: Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>,
    pub nullable_iface: Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>,
    pub iface_list: Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>,
    pub nullable_iface_list: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>,
  }
  impl Default for MyParcelable {
    fn default() -> Self {
      Self {
        iface: Default::default(),
        nullable_iface: Default::default(),
        iface_list: Default::default(),
        nullable_iface_list: Default::default(),
      }
    }
  }
  impl binder::Parcelable for MyParcelable {
    fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      parcel.sized_write(|subparcel| {
        let __field_ref = self.iface.as_ref().ok_or(binder::StatusCode::UNEXPECTED_NULL)?;
        subparcel.write(__field_ref)?;
        subparcel.write(&self.nullable_iface)?;
        subparcel.write(&self.iface_list)?;
        subparcel.write(&self.nullable_iface_list)?;
        Ok(())
      })
    }
    fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      parcel.sized_read(|subparcel| {
        if subparcel.has_more_data() {
          self.iface = Some(subparcel.read()?);
        }
        if subparcel.has_more_data() {
          self.nullable_iface = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.iface_list = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.nullable_iface_list = subparcel.read()?;
        }
        Ok(())
      })
    }
  }
  binder::impl_serialize_for_parcelable!(MyParcelable);
  binder::impl_deserialize_for_parcelable!(MyParcelable);
  impl binder::binder_impl::ParcelableMetadata for MyParcelable {
    fn get_descriptor() -> &'static str { "android.aidl.tests.ListOfInterfaces.MyParcelable" }
  }
}
pub mod MyUnion {
  #[derive(Debug)]
  pub enum MyUnion {
    Iface(Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>),
    Nullable_iface(Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>),
    Iface_list(Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>),
    Nullable_iface_list(Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>>),
  }
  impl Default for MyUnion {
    fn default() -> Self {
      Self::Iface(Default::default())
    }
  }
  impl binder::Parcelable for MyUnion {
    fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      match self {
        Self::Iface(v) => {
          parcel.write(&0i32)?;
          let __field_ref = v.as_ref().ok_or(binder::StatusCode::UNEXPECTED_NULL)?;
          parcel.write(__field_ref)
        }
        Self::Nullable_iface(v) => {
          parcel.write(&1i32)?;
          parcel.write(v)
        }
        Self::Iface_list(v) => {
          parcel.write(&2i32)?;
          parcel.write(v)
        }
        Self::Nullable_iface_list(v) => {
          parcel.write(&3i32)?;
          parcel.write(v)
        }
      }
    }
    fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      let tag: i32 = parcel.read()?;
      match tag {
        0 => {
          let value: Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = Some(parcel.read()?);
          *self = Self::Iface(value);
          Ok(())
        }
        1 => {
          let value: Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = parcel.read()?;
          *self = Self::Nullable_iface(value);
          Ok(())
        }
        2 => {
          let value: Vec<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>> = parcel.read()?;
          *self = Self::Iface_list(value);
          Ok(())
        }
        3 => {
          let value: Option<Vec<Option<binder::Strong<dyn crate::mangled::_7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface>>>> = parcel.read()?;
          *self = Self::Nullable_iface_list(value);
          Ok(())
        }
        _ => {
          Err(binder::StatusCode::BAD_VALUE)
        }
      }
    }
  }
  binder::impl_serialize_for_parcelable!(MyUnion);
  binder::impl_deserialize_for_parcelable!(MyUnion);
  impl binder::binder_impl::ParcelableMetadata for MyUnion {
    fn get_descriptor() -> &'static str { "android.aidl.tests.ListOfInterfaces.MyUnion" }
  }
  pub mod Tag {
    #![allow(non_upper_case_globals)]
    use binder::declare_binder_enum;
    declare_binder_enum! {
      Tag : [i32; 4] {
        iface = 0,
        nullable_iface = 1,
        iface_list = 2,
        nullable_iface_list = 3,
      }
    }
  }
}
pub(crate) mod mangled {
 pub use super::ListOfInterfaces as _7_android_4_aidl_5_tests_16_ListOfInterfaces;
 pub use super::IEmptyInterface::IEmptyInterface as _7_android_4_aidl_5_tests_16_ListOfInterfaces_15_IEmptyInterface;
 pub use super::IMyInterface::IMyInterface as _7_android_4_aidl_5_tests_16_ListOfInterfaces_12_IMyInterface;
 pub use super::MyParcelable::MyParcelable as _7_android_4_aidl_5_tests_16_ListOfInterfaces_12_MyParcelable;
 pub use super::MyUnion::MyUnion as _7_android_4_aidl_5_tests_16_ListOfInterfaces_7_MyUnion;
 pub use super::MyUnion::Tag::Tag as _7_android_4_aidl_5_tests_16_ListOfInterfaces_7_MyUnion_3_Tag;
}

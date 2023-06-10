#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct RecursiveList {
  pub value: i32,
  pub next: Option<Box<crate::mangled::_7_android_4_aidl_5_tests_13_RecursiveList>>,
}
impl Default for RecursiveList {
  fn default() -> Self {
    Self {
      value: 0,
      next: Default::default(),
    }
  }
}
impl binder::Parcelable for RecursiveList {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.value)?;
      subparcel.write(&self.next)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.value = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.next = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(RecursiveList);
binder::impl_deserialize_for_parcelable!(RecursiveList);
impl binder::binder_impl::ParcelableMetadata for RecursiveList {
  fn get_descriptor() -> &'static str { "android.aidl.tests.RecursiveList" }
}
pub(crate) mod mangled {
 pub use super::RecursiveList as _7_android_4_aidl_5_tests_13_RecursiveList;
}

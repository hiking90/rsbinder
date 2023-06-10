#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct OtherParcelableForToString {
  pub field: String,
}
impl Default for OtherParcelableForToString {
  fn default() -> Self {
    Self {
      field: Default::default(),
    }
  }
}
impl binder::Parcelable for OtherParcelableForToString {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.field)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.field = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(OtherParcelableForToString);
binder::impl_deserialize_for_parcelable!(OtherParcelableForToString);
impl binder::binder_impl::ParcelableMetadata for OtherParcelableForToString {
  fn get_descriptor() -> &'static str { "android.aidl.tests.OtherParcelableForToString" }
}
pub(crate) mod mangled {
 pub use super::OtherParcelableForToString as _7_android_4_aidl_5_tests_26_OtherParcelableForToString;
}

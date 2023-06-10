#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
#[deprecated = "test"]
pub struct DeprecatedParcelable {
}
impl Default for DeprecatedParcelable {
  fn default() -> Self {
    Self {
    }
  }
}
impl binder::Parcelable for DeprecatedParcelable {
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
binder::impl_serialize_for_parcelable!(DeprecatedParcelable);
binder::impl_deserialize_for_parcelable!(DeprecatedParcelable);
impl binder::binder_impl::ParcelableMetadata for DeprecatedParcelable {
  fn get_descriptor() -> &'static str { "android.aidl.tests.DeprecatedParcelable" }
}
pub(crate) mod mangled {
 pub use super::DeprecatedParcelable as _7_android_4_aidl_5_tests_20_DeprecatedParcelable;
}

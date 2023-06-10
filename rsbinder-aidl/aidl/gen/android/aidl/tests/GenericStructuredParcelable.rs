#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct GenericStructuredParcelable {
  pub a: i32,
  pub b: i32,
}
impl Default for GenericStructuredParcelable {
  fn default() -> Self {
    Self {
      a: 0,
      b: 0,
    }
  }
}
impl binder::Parcelable for GenericStructuredParcelable {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.a)?;
      subparcel.write(&self.b)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.a = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.b = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(GenericStructuredParcelable);
binder::impl_deserialize_for_parcelable!(GenericStructuredParcelable);
impl binder::binder_impl::ParcelableMetadata for GenericStructuredParcelable {
  fn get_descriptor() -> &'static str { "android.aidl.tests.GenericStructuredParcelable" }
}
pub(crate) mod mangled {
 pub use super::GenericStructuredParcelable as _7_android_4_aidl_5_tests_27_GenericStructuredParcelable;
}

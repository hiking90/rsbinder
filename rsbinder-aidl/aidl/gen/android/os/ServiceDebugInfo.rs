#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct ServiceDebugInfo {
  pub name: String,
  pub debugPid: i32,
}
impl Default for ServiceDebugInfo {
  fn default() -> Self {
    Self {
      name: Default::default(),
      debugPid: 0,
    }
  }
}
impl binder::Parcelable for ServiceDebugInfo {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.name)?;
      subparcel.write(&self.debugPid)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.name = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.debugPid = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(ServiceDebugInfo);
binder::impl_deserialize_for_parcelable!(ServiceDebugInfo);
impl binder::binder_impl::ParcelableMetadata for ServiceDebugInfo {
  fn get_descriptor() -> &'static str { "android.os.ServiceDebugInfo" }
}
pub(crate) mod mangled {
 pub use super::ServiceDebugInfo as _7_android_2_os_16_ServiceDebugInfo;
}

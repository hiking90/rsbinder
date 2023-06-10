#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct ConnectionInfo {
  pub ipAddress: String,
  pub port: i32,
}
impl Default for ConnectionInfo {
  fn default() -> Self {
    Self {
      ipAddress: Default::default(),
      port: 0,
    }
  }
}
impl binder::Parcelable for ConnectionInfo {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.ipAddress)?;
      subparcel.write(&self.port)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.ipAddress = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.port = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(ConnectionInfo);
binder::impl_deserialize_for_parcelable!(ConnectionInfo);
impl binder::binder_impl::ParcelableMetadata for ConnectionInfo {
  fn get_descriptor() -> &'static str { "android.os.ConnectionInfo" }
}
pub(crate) mod mangled {
 pub use super::ConnectionInfo as _7_android_2_os_14_ConnectionInfo;
}

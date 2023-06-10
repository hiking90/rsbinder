#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct ParcelableForToString {
  pub intValue: i32,
  pub intArray: Vec<i32>,
  pub longValue: i64,
  pub longArray: Vec<i64>,
  pub doubleValue: f64,
  pub doubleArray: Vec<f64>,
  pub floatValue: f32,
  pub floatArray: Vec<f32>,
  pub byteValue: i8,
  pub byteArray: Vec<u8>,
  pub booleanValue: bool,
  pub booleanArray: Vec<bool>,
  pub stringValue: String,
  pub stringArray: Vec<String>,
  pub stringList: Vec<String>,
  pub parcelableValue: crate::mangled::_7_android_4_aidl_5_tests_26_OtherParcelableForToString,
  pub parcelableArray: Vec<crate::mangled::_7_android_4_aidl_5_tests_26_OtherParcelableForToString>,
  pub enumValue: crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum,
  pub enumArray: Vec<crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum>,
  pub nullArray: Vec<String>,
  pub nullList: Vec<String>,
  pub parcelableGeneric: i32,
  pub unionValue: crate::mangled::_7_android_4_aidl_5_tests_5_Union,
}
impl Default for ParcelableForToString {
  fn default() -> Self {
    Self {
      intValue: 0,
      intArray: Default::default(),
      longValue: 0,
      longArray: Default::default(),
      doubleValue: 0.000000f64,
      doubleArray: Default::default(),
      floatValue: 0.000000f32,
      floatArray: Default::default(),
      byteValue: 0,
      byteArray: Default::default(),
      booleanValue: false,
      booleanArray: Default::default(),
      stringValue: Default::default(),
      stringArray: Default::default(),
      stringList: Default::default(),
      parcelableValue: Default::default(),
      parcelableArray: Default::default(),
      enumValue: crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum::FOO,
      enumArray: Default::default(),
      nullArray: Default::default(),
      nullList: Default::default(),
      parcelableGeneric: Default::default(),
      unionValue: Default::default(),
    }
  }
}
impl binder::Parcelable for ParcelableForToString {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.intValue)?;
      subparcel.write(&self.intArray)?;
      subparcel.write(&self.longValue)?;
      subparcel.write(&self.longArray)?;
      subparcel.write(&self.doubleValue)?;
      subparcel.write(&self.doubleArray)?;
      subparcel.write(&self.floatValue)?;
      subparcel.write(&self.floatArray)?;
      subparcel.write(&self.byteValue)?;
      subparcel.write(&self.byteArray)?;
      subparcel.write(&self.booleanValue)?;
      subparcel.write(&self.booleanArray)?;
      subparcel.write(&self.stringValue)?;
      subparcel.write(&self.stringArray)?;
      subparcel.write(&self.stringList)?;
      subparcel.write(&self.parcelableValue)?;
      subparcel.write(&self.parcelableArray)?;
      subparcel.write(&self.enumValue)?;
      subparcel.write(&self.enumArray)?;
      subparcel.write(&self.nullArray)?;
      subparcel.write(&self.nullList)?;
      subparcel.write(&self.parcelableGeneric)?;
      subparcel.write(&self.unionValue)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.intValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.intArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.longValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.longArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.doubleValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.doubleArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.floatValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.floatArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.byteValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.byteArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.booleanValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.booleanArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.stringValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.stringArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.stringList = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.parcelableValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.parcelableArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.enumValue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.enumArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.nullArray = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.nullList = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.parcelableGeneric = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.unionValue = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(ParcelableForToString);
binder::impl_deserialize_for_parcelable!(ParcelableForToString);
impl binder::binder_impl::ParcelableMetadata for ParcelableForToString {
  fn get_descriptor() -> &'static str { "android.aidl.tests.ParcelableForToString" }
}
pub(crate) mod mangled {
 pub use super::ParcelableForToString as _7_android_4_aidl_5_tests_21_ParcelableForToString;
}

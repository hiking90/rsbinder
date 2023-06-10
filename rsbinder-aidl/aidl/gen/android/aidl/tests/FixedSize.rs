#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug)]
pub struct FixedSize {
}
impl Default for FixedSize {
  fn default() -> Self {
    Self {
    }
  }
}
impl binder::Parcelable for FixedSize {
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
binder::impl_serialize_for_parcelable!(FixedSize);
binder::impl_deserialize_for_parcelable!(FixedSize);
impl binder::binder_impl::ParcelableMetadata for FixedSize {
  fn get_descriptor() -> &'static str { "android.aidl.tests.FixedSize" }
}
pub mod FixedParcelable {
  #[derive(Debug)]
  pub struct FixedParcelable {
    pub booleanValue: bool,
    pub byteValue: i8,
    pub charValue: u16,
    pub intValue: i32,
    pub longValue: i64,
    pub floatValue: f32,
    pub doubleValue: f64,
    pub enumValue: crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum,
    pub parcelableValue: crate::mangled::_7_android_4_aidl_5_tests_9_FixedSize_10_FixedUnion,
  }
  impl Default for FixedParcelable {
    fn default() -> Self {
      Self {
        booleanValue: false,
        byteValue: 0,
        charValue: '\0' as u16,
        intValue: 0,
        longValue: 0,
        floatValue: 0.000000f32,
        doubleValue: 0.000000f64,
        enumValue: crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum::FOO,
        parcelableValue: Default::default(),
      }
    }
  }
  impl binder::Parcelable for FixedParcelable {
    fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      parcel.sized_write(|subparcel| {
        subparcel.write(&self.booleanValue)?;
        subparcel.write(&self.byteValue)?;
        subparcel.write(&self.charValue)?;
        subparcel.write(&self.intValue)?;
        subparcel.write(&self.longValue)?;
        subparcel.write(&self.floatValue)?;
        subparcel.write(&self.doubleValue)?;
        subparcel.write(&self.enumValue)?;
        subparcel.write(&self.parcelableValue)?;
        Ok(())
      })
    }
    fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      parcel.sized_read(|subparcel| {
        if subparcel.has_more_data() {
          self.booleanValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.byteValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.charValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.intValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.longValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.floatValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.doubleValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.enumValue = subparcel.read()?;
        }
        if subparcel.has_more_data() {
          self.parcelableValue = subparcel.read()?;
        }
        Ok(())
      })
    }
  }
  binder::impl_serialize_for_parcelable!(FixedParcelable);
  binder::impl_deserialize_for_parcelable!(FixedParcelable);
  impl binder::binder_impl::ParcelableMetadata for FixedParcelable {
    fn get_descriptor() -> &'static str { "android.aidl.tests.FixedSize.FixedParcelable" }
  }
}
pub mod FixedUnion {
  #[derive(Debug)]
  pub enum FixedUnion {
    BooleanValue(bool),
    ByteValue(i8),
    CharValue(u16),
    IntValue(i32),
    LongValue(i64),
    FloatValue(f32),
    DoubleValue(f64),
    EnumValue(crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum),
  }
  impl Default for FixedUnion {
    fn default() -> Self {
      Self::BooleanValue(false)
    }
  }
  impl binder::Parcelable for FixedUnion {
    fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      match self {
        Self::BooleanValue(v) => {
          parcel.write(&0i32)?;
          parcel.write(v)
        }
        Self::ByteValue(v) => {
          parcel.write(&1i32)?;
          parcel.write(v)
        }
        Self::CharValue(v) => {
          parcel.write(&2i32)?;
          parcel.write(v)
        }
        Self::IntValue(v) => {
          parcel.write(&3i32)?;
          parcel.write(v)
        }
        Self::LongValue(v) => {
          parcel.write(&4i32)?;
          parcel.write(v)
        }
        Self::FloatValue(v) => {
          parcel.write(&5i32)?;
          parcel.write(v)
        }
        Self::DoubleValue(v) => {
          parcel.write(&6i32)?;
          parcel.write(v)
        }
        Self::EnumValue(v) => {
          parcel.write(&7i32)?;
          parcel.write(v)
        }
      }
    }
    fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
      let tag: i32 = parcel.read()?;
      match tag {
        0 => {
          let value: bool = parcel.read()?;
          *self = Self::BooleanValue(value);
          Ok(())
        }
        1 => {
          let value: i8 = parcel.read()?;
          *self = Self::ByteValue(value);
          Ok(())
        }
        2 => {
          let value: u16 = parcel.read()?;
          *self = Self::CharValue(value);
          Ok(())
        }
        3 => {
          let value: i32 = parcel.read()?;
          *self = Self::IntValue(value);
          Ok(())
        }
        4 => {
          let value: i64 = parcel.read()?;
          *self = Self::LongValue(value);
          Ok(())
        }
        5 => {
          let value: f32 = parcel.read()?;
          *self = Self::FloatValue(value);
          Ok(())
        }
        6 => {
          let value: f64 = parcel.read()?;
          *self = Self::DoubleValue(value);
          Ok(())
        }
        7 => {
          let value: crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum = parcel.read()?;
          *self = Self::EnumValue(value);
          Ok(())
        }
        _ => {
          Err(binder::StatusCode::BAD_VALUE)
        }
      }
    }
  }
  binder::impl_serialize_for_parcelable!(FixedUnion);
  binder::impl_deserialize_for_parcelable!(FixedUnion);
  impl binder::binder_impl::ParcelableMetadata for FixedUnion {
    fn get_descriptor() -> &'static str { "android.aidl.tests.FixedSize.FixedUnion" }
  }
  pub mod Tag {
    #![allow(non_upper_case_globals)]
    use binder::declare_binder_enum;
    declare_binder_enum! {
      Tag : [i8; 8] {
        booleanValue = 0,
        byteValue = 1,
        charValue = 2,
        intValue = 3,
        longValue = 4,
        floatValue = 5,
        doubleValue = 6,
        enumValue = 7,
      }
    }
  }
}
pub(crate) mod mangled {
 pub use super::FixedSize as _7_android_4_aidl_5_tests_9_FixedSize;
 pub use super::FixedParcelable::FixedParcelable as _7_android_4_aidl_5_tests_9_FixedSize_15_FixedParcelable;
 pub use super::FixedUnion::FixedUnion as _7_android_4_aidl_5_tests_9_FixedSize_10_FixedUnion;
 pub use super::FixedUnion::Tag::Tag as _7_android_4_aidl_5_tests_9_FixedSize_10_FixedUnion_3_Tag;
}

#![forbid(unsafe_code)]
#![rustfmt::skip]
#[derive(Debug, Clone, PartialEq)]
pub struct StructuredParcelable {
  pub shouldContainThreeFs: Vec<i32>,
  pub f: i32,
  pub shouldBeJerry: String,
  pub shouldBeByteBar: crate::mangled::_7_android_4_aidl_5_tests_8_ByteEnum,
  pub shouldBeIntBar: crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum,
  pub shouldBeLongBar: crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum,
  pub shouldContainTwoByteFoos: Vec<crate::mangled::_7_android_4_aidl_5_tests_8_ByteEnum>,
  pub shouldContainTwoIntFoos: Vec<crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum>,
  pub shouldContainTwoLongFoos: Vec<crate::mangled::_7_android_4_aidl_5_tests_8_LongEnum>,
  pub stringDefaultsToFoo: String,
  pub byteDefaultsToFour: i8,
  pub intDefaultsToFive: i32,
  pub longDefaultsToNegativeSeven: i64,
  pub booleanDefaultsToTrue: bool,
  pub charDefaultsToC: u16,
  pub floatDefaultsToPi: f32,
  pub doubleWithDefault: f64,
  pub arrayDefaultsTo123: Vec<i32>,
  pub arrayDefaultsToEmpty: Vec<i32>,
  pub boolDefault: bool,
  pub byteDefault: i8,
  pub intDefault: i32,
  pub longDefault: i64,
  pub floatDefault: f32,
  pub doubleDefault: f64,
  pub checkDoubleFromFloat: f64,
  pub checkStringArray1: Vec<String>,
  pub checkStringArray2: Vec<String>,
  pub int32_min: i32,
  pub int32_max: i32,
  pub int64_max: i64,
  pub hexInt32_neg_1: i32,
  pub ibinder: Option<binder::SpIBinder>,
  pub empty: crate::mangled::_7_android_4_aidl_5_tests_20_StructuredParcelable_5_Empty,
  pub int8_1: Vec<u8>,
  pub int32_1: Vec<i32>,
  pub int64_1: Vec<i64>,
  pub hexInt32_pos_1: i32,
  pub hexInt64_pos_1: i32,
  pub const_exprs_1: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_2: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_3: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_4: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_5: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_6: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_7: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_8: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_9: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub const_exprs_10: crate::mangled::_7_android_4_aidl_5_tests_22_ConstantExpressionEnum,
  pub addString1: String,
  pub addString2: String,
  pub shouldSetBit0AndBit2: i32,
  pub u: Option<crate::mangled::_7_android_4_aidl_5_tests_5_Union>,
  pub shouldBeConstS1: Option<crate::mangled::_7_android_4_aidl_5_tests_5_Union>,
  pub defaultWithFoo: crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum,
}
pub const BIT0: i32 = 1;
pub const BIT1: i32 = 2;
pub const BIT2: i32 = 4;
impl Default for StructuredParcelable {
  fn default() -> Self {
    Self {
      shouldContainThreeFs: Default::default(),
      f: 0,
      shouldBeJerry: Default::default(),
      shouldBeByteBar: Default::default(),
      shouldBeIntBar: Default::default(),
      shouldBeLongBar: Default::default(),
      shouldContainTwoByteFoos: Default::default(),
      shouldContainTwoIntFoos: Default::default(),
      shouldContainTwoLongFoos: Default::default(),
      stringDefaultsToFoo: "foo".into(),
      byteDefaultsToFour: 4,
      intDefaultsToFive: 5,
      longDefaultsToNegativeSeven: -7,
      booleanDefaultsToTrue: true,
      charDefaultsToC: 'C' as u16,
      floatDefaultsToPi: 3.140000f32,
      doubleWithDefault: -314000000000000000.000000f64,
      arrayDefaultsTo123: vec![1, 2, 3],
      arrayDefaultsToEmpty: vec![],
      boolDefault: false,
      byteDefault: 0,
      intDefault: 0,
      longDefault: 0,
      floatDefault: 0.000000f32,
      doubleDefault: 0.000000f64,
      checkDoubleFromFloat: 3.140000f64,
      checkStringArray1: vec!["a".into(), "b".into()],
      checkStringArray2: vec!["a".into(), "b".into()],
      int32_min: -2147483648,
      int32_max: 2147483647,
      int64_max: 9223372036854775807,
      hexInt32_neg_1: -1,
      ibinder: Default::default(),
      empty: Default::default(),
      int8_1: vec![1, 1, 1, 1, 1],
      int32_1: vec![1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
      int64_1: vec![1, 1, 1, 1, 1, 1, 1, 1, 1, 1],
      hexInt32_pos_1: 1,
      hexInt64_pos_1: 1,
      const_exprs_1: Default::default(),
      const_exprs_2: Default::default(),
      const_exprs_3: Default::default(),
      const_exprs_4: Default::default(),
      const_exprs_5: Default::default(),
      const_exprs_6: Default::default(),
      const_exprs_7: Default::default(),
      const_exprs_8: Default::default(),
      const_exprs_9: Default::default(),
      const_exprs_10: Default::default(),
      addString1: "hello world!".into(),
      addString2: "The quick brown fox jumps over the lazy dog.".into(),
      shouldSetBit0AndBit2: 0,
      u: Default::default(),
      shouldBeConstS1: Default::default(),
      defaultWithFoo: crate::mangled::_7_android_4_aidl_5_tests_7_IntEnum::FOO,
    }
  }
}
impl binder::Parcelable for StructuredParcelable {
  fn write_to_parcel(&self, parcel: &mut binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_write(|subparcel| {
      subparcel.write(&self.shouldContainThreeFs)?;
      subparcel.write(&self.f)?;
      subparcel.write(&self.shouldBeJerry)?;
      subparcel.write(&self.shouldBeByteBar)?;
      subparcel.write(&self.shouldBeIntBar)?;
      subparcel.write(&self.shouldBeLongBar)?;
      subparcel.write(&self.shouldContainTwoByteFoos)?;
      subparcel.write(&self.shouldContainTwoIntFoos)?;
      subparcel.write(&self.shouldContainTwoLongFoos)?;
      subparcel.write(&self.stringDefaultsToFoo)?;
      subparcel.write(&self.byteDefaultsToFour)?;
      subparcel.write(&self.intDefaultsToFive)?;
      subparcel.write(&self.longDefaultsToNegativeSeven)?;
      subparcel.write(&self.booleanDefaultsToTrue)?;
      subparcel.write(&self.charDefaultsToC)?;
      subparcel.write(&self.floatDefaultsToPi)?;
      subparcel.write(&self.doubleWithDefault)?;
      subparcel.write(&self.arrayDefaultsTo123)?;
      subparcel.write(&self.arrayDefaultsToEmpty)?;
      subparcel.write(&self.boolDefault)?;
      subparcel.write(&self.byteDefault)?;
      subparcel.write(&self.intDefault)?;
      subparcel.write(&self.longDefault)?;
      subparcel.write(&self.floatDefault)?;
      subparcel.write(&self.doubleDefault)?;
      subparcel.write(&self.checkDoubleFromFloat)?;
      subparcel.write(&self.checkStringArray1)?;
      subparcel.write(&self.checkStringArray2)?;
      subparcel.write(&self.int32_min)?;
      subparcel.write(&self.int32_max)?;
      subparcel.write(&self.int64_max)?;
      subparcel.write(&self.hexInt32_neg_1)?;
      subparcel.write(&self.ibinder)?;
      subparcel.write(&self.empty)?;
      subparcel.write(&self.int8_1)?;
      subparcel.write(&self.int32_1)?;
      subparcel.write(&self.int64_1)?;
      subparcel.write(&self.hexInt32_pos_1)?;
      subparcel.write(&self.hexInt64_pos_1)?;
      subparcel.write(&self.const_exprs_1)?;
      subparcel.write(&self.const_exprs_2)?;
      subparcel.write(&self.const_exprs_3)?;
      subparcel.write(&self.const_exprs_4)?;
      subparcel.write(&self.const_exprs_5)?;
      subparcel.write(&self.const_exprs_6)?;
      subparcel.write(&self.const_exprs_7)?;
      subparcel.write(&self.const_exprs_8)?;
      subparcel.write(&self.const_exprs_9)?;
      subparcel.write(&self.const_exprs_10)?;
      subparcel.write(&self.addString1)?;
      subparcel.write(&self.addString2)?;
      subparcel.write(&self.shouldSetBit0AndBit2)?;
      subparcel.write(&self.u)?;
      subparcel.write(&self.shouldBeConstS1)?;
      subparcel.write(&self.defaultWithFoo)?;
      Ok(())
    })
  }
  fn read_from_parcel(&mut self, parcel: &binder::binder_impl::BorrowedParcel) -> std::result::Result<(), binder::StatusCode> {
    parcel.sized_read(|subparcel| {
      if subparcel.has_more_data() {
        self.shouldContainThreeFs = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.f = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldBeJerry = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldBeByteBar = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldBeIntBar = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldBeLongBar = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldContainTwoByteFoos = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldContainTwoIntFoos = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldContainTwoLongFoos = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.stringDefaultsToFoo = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.byteDefaultsToFour = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.intDefaultsToFive = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.longDefaultsToNegativeSeven = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.booleanDefaultsToTrue = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.charDefaultsToC = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.floatDefaultsToPi = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.doubleWithDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.arrayDefaultsTo123 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.arrayDefaultsToEmpty = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.boolDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.byteDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.intDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.longDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.floatDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.doubleDefault = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.checkDoubleFromFloat = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.checkStringArray1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.checkStringArray2 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int32_min = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int32_max = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int64_max = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.hexInt32_neg_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.ibinder = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.empty = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int8_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int32_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.int64_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.hexInt32_pos_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.hexInt64_pos_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_2 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_3 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_4 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_5 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_6 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_7 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_8 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_9 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.const_exprs_10 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.addString1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.addString2 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldSetBit0AndBit2 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.u = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.shouldBeConstS1 = subparcel.read()?;
      }
      if subparcel.has_more_data() {
        self.defaultWithFoo = subparcel.read()?;
      }
      Ok(())
    })
  }
}
binder::impl_serialize_for_parcelable!(StructuredParcelable);
binder::impl_deserialize_for_parcelable!(StructuredParcelable);
impl binder::binder_impl::ParcelableMetadata for StructuredParcelable {
  fn get_descriptor() -> &'static str { "android.aidl.tests.StructuredParcelable" }
}
pub mod Empty {
  #[derive(Debug, Clone, PartialEq)]
  pub struct Empty {
  }
  impl Default for Empty {
    fn default() -> Self {
      Self {
      }
    }
  }
  impl binder::Parcelable for Empty {
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
  binder::impl_serialize_for_parcelable!(Empty);
  binder::impl_deserialize_for_parcelable!(Empty);
  impl binder::binder_impl::ParcelableMetadata for Empty {
    fn get_descriptor() -> &'static str { "android.aidl.tests.StructuredParcelable.Empty" }
  }
}
pub(crate) mod mangled {
 pub use super::StructuredParcelable as _7_android_4_aidl_5_tests_20_StructuredParcelable;
 pub use super::Empty::Empty as _7_android_4_aidl_5_tests_20_StructuredParcelable_5_Empty;
}

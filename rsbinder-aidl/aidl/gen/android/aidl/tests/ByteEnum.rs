#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  ByteEnum : [i8; 3] {
    FOO = 1,
    BAR = 2,
    BAZ = 3,
  }
}
pub(crate) mod mangled {
 pub use super::ByteEnum as _7_android_4_aidl_5_tests_8_ByteEnum;
}

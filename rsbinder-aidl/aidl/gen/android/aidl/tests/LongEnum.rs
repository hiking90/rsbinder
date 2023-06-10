#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  LongEnum : [i64; 3] {
    FOO = 100000000000,
    BAR = 200000000000,
    BAZ = 200000000001,
  }
}
pub(crate) mod mangled {
 pub use super::LongEnum as _7_android_4_aidl_5_tests_8_LongEnum;
}

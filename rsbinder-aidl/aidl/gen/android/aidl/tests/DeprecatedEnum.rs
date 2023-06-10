#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  #[deprecated = "test"]
  DeprecatedEnum : [i32; 3] {
    A = 0,
    B = 1,
    C = 2,
  }
}
pub(crate) mod mangled {
 pub use super::DeprecatedEnum as _7_android_4_aidl_5_tests_14_DeprecatedEnum;
}

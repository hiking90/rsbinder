#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  ConstantExpressionEnum : [i32; 10] {
    decInt32_1 = 1,
    decInt32_2 = 1,
    decInt64_1 = 1,
    decInt64_2 = 1,
    decInt64_3 = 1,
    decInt64_4 = 1,
    hexInt32_1 = 1,
    hexInt32_2 = 1,
    hexInt32_3 = 1,
    hexInt64_1 = 1,
  }
}
pub(crate) mod mangled {
 pub use super::ConstantExpressionEnum as _7_android_4_aidl_5_tests_22_ConstantExpressionEnum;
}

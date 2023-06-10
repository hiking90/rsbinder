#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  IntEnum : [i32; 4] {
    FOO = 1000,
    BAR = 2000,
    BAZ = 2001,
    #[deprecated = "do not use this"]
    QUX = 2002,
  }
}
pub(crate) mod mangled {
 pub use super::IntEnum as _7_android_4_aidl_5_tests_7_IntEnum;
}

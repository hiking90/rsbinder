#![forbid(unsafe_code)]
#![rustfmt::skip]
#![allow(non_upper_case_globals)]
use binder::declare_binder_enum;
declare_binder_enum! {
  BackendType : [i8; 4] {
    CPP = 0,
    JAVA = 1,
    NDK = 2,
    RUST = 3,
  }
}
pub(crate) mod mangled {
 pub use super::BackendType as _7_android_4_aidl_5_tests_11_BackendType;
}

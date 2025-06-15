# rsbinder-aidl
This is an AIDL compiler for **rsbinder**.

## How to use the AIDL Code Generator
* Add the build-dependencies to Cargo.toml:
```
[build-dependencies]
rsbinder-aidl = "0.4.0"
```
* Create a build.rs file in the root folder of the crate.
* Add use std::path::PathBuf; to build.rs.
* Add the following content:
```
rsbinder_aidl::Builder::new()
    .source(PathBuf::from("aidl/....")
    .source(PathBuf::from("aidl/....")
    .source(PathBuf::from("aidl/....")
    .output(PathBuf::from("aidl_name.rs")
    .generate().unwrap()
```
## How to create AIDL file
Please read Android AIDL documents.

https://source.android.com/docs/core/architecture/aidl

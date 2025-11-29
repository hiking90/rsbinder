# rsbinder-aidl
This is an AIDL compiler for **rsbinder**.

## How to use the AIDL Code Generator
Add dependencies to Cargo.toml:
```toml
[dependencies]
rsbinder = "0.5"

[build-dependencies]
rsbinder-aidl = { version = "0.5", features = ["async"] }
```

Create a build.rs file:
```rust
use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/IMyService.aidl"))
        .output(PathBuf::from("my_service.rs"))
        .generate()
        .unwrap();
}
```

### Sync-only Setup
For environments without async runtime:
```toml
[dependencies]
rsbinder = { version = "0.5", default-features = false }

[build-dependencies]
rsbinder-aidl = "0.5"
```

## How to create AIDL file
Please read Android AIDL documents.

https://source.android.com/docs/core/architecture/aidl

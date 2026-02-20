# rsbinder-aidl
This is an AIDL compiler for **rsbinder**.

## How to use the AIDL Code Generator
Add dependencies to Cargo.toml:
```toml
[dependencies]
rsbinder = "0.6"

[build-dependencies]
rsbinder-aidl = { version = "0.6", features = ["async"] }
```

Create a build.rs file:
```rust
use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("aidl/IMyService.aidl"))
        .output(PathBuf::from("my_service.rs"))
        .generate()
        .unwrap_or_else(|err| {
            eprintln!("{:?}", miette::Report::new(err));
            std::process::exit(1);
        });
}
```

### Sync-only Setup
For environments without async runtime:
```toml
[dependencies]
rsbinder = { version = "0.6", default-features = false }

[build-dependencies]
rsbinder-aidl = "0.6"
```

## Error Reporting

`rsbinder-aidl` uses [miette](https://crates.io/crates/miette) for structured error reporting.
When an AIDL file contains a syntax or semantic error, the compiler reports the file name,
line number, source snippet, and a helpful message:

```
  × AIDL Parse Error [aidl::parse_error]
  ╭─[hello.aidl:5:12]
4 │ interface IHello {
5 │     void 123bad();
  ·          ^^^^^^ expected identifier
6 │ }
  ╰────
  help: method names must start with a letter or underscore
```

To enable fancy (colored, Unicode) output in a binary, call `miette::set_hook()` at startup:
```rust
fn main() -> miette::Result<()> {
    miette::set_hook(Box::new(|_| {
        Box::new(miette::MietteHandlerOpts::new().build())
    }))?;

    rsbinder_aidl::Builder::new()
        .source(std::path::PathBuf::from("aidl/IMyService.aidl"))
        .output(std::path::PathBuf::from("my_service.rs"))
        .generate()?;
    Ok(())
}
```

In a `build.rs`, simply wrap the error with `miette::Report` and print to stderr:
```rust
fn main() {
    if let Err(err) = rsbinder_aidl::Builder::new()
        .source(std::path::PathBuf::from("aidl/IMyService.aidl"))
        .output(std::path::PathBuf::from("my_service.rs"))
        .generate()
    {
        eprintln!("{:?}", miette::Report::new(err));
        std::process::exit(1);
    }
}
```

## How to create AIDL file
Please read Android AIDL documents.

https://source.android.com/docs/core/architecture/aidl

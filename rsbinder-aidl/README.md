# rsbinder-aidl
This is an AIDL compiler for **rsbinder**.

## How to use the AIDL Code Generator
Add dependencies to Cargo.toml (check crates.io for the latest version):
```toml
[dependencies]
rsbinder = "0.9"

[build-dependencies]
rsbinder-aidl = { version = "0.9", features = ["async"] }
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

Then include the generated code in your crate with rsbinder's `include_aidl!`
macro (first argument = the `output` file stem, second = the path to import):
```rust
rsbinder::include_aidl!("my_service", crate::IMyService::*);
```

### Builder options
- `.source(path)` — add a `.aidl` file, or a directory that is scanned
  recursively for `*.aidl`. May be called multiple times.
- `.include_dir(path)` — add an import search directory (the AOSP `-I`
  equivalent). All directories are scanned deterministically; an import
  found under more than one directory is an error, matching AOSP.
- `.output(name)` — the generated file name, written under `OUT_DIR`.
- `.version(n)` / `.hash("…")` — stamp the **most recently added file
  source** with stable-AIDL version metadata (AOSP `aidl --version N
  --hash <s>` equivalent); the generated interfaces gain
  `getInterfaceVersion()` / `getInterfaceHash()` meta methods.
- `.set_async_support(bool)` — also emit `.await`-able async client/server
  traits (defaults to the crate's `async` feature).

### Sync-only Setup
For environments without async runtime:
```toml
[dependencies]
rsbinder = { version = "0.9", default-features = false }

[build-dependencies]
rsbinder-aidl = "0.9"
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

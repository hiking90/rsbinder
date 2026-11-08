# rsbinder

[![crates.io](https://img.shields.io/crates/v/rsbinder.svg)](https://crates.io/crates/rsbinder)
[![Docs.rs](https://docs.rs/rsbinder/badge.svg)](https://docs.rs/rsbinder)
[![Rust Version](https://img.shields.io/badge/rustc-1.86+-blue.svg)](https://blog.rust-lang.org/)
[![GitHub](https://img.shields.io/badge/github-hiking90%2Frsbinder-8da0cb?logo=github)](https://github.com/hiking90/rsbinder)

Android's Binder IPC in pure Rust, for Linux, Android and macOS. **rsbinder is
compatible with Android at the protocol level**: an rsbinder client calls the
C++ and Java services of an Android 10–17 device, and an rsbinder service
answers their clients, with no `libbinder` linked (see
[Android compatibility](#android-compatibility)).

An interface generated from `.aidl` by
[`rsbinder-aidl`](https://crates.io/crates/rsbinder-aidl), or declared as a
Rust trait with the `macros` feature, is served and called over either of two
transports, chosen at run time by a `Uri`:

- **Kernel binder** — `/dev/binder` on Android, and on Linux through binderfs.
- **RPC transport** (feature `rpc`) — binder over a Unix, vsock or TLS socket,
  with no kernel driver and no root.

## Usage

```toml
[dependencies]
rsbinder = "0.13"

[build-dependencies]
rsbinder-aidl = { version = "0.13", features = ["async"] }
```

`build.rs` generates the Rust code for an interface:

```rust,ignore
fn main() {
    rsbinder_aidl::Builder::new()
        .source(std::path::PathBuf::from("aidl/hello/IHello.aidl"))
        .output(std::path::PathBuf::from("hello.rs"))
        .generate()
        .unwrap();
}
```

A service and a client of it:

```rust,ignore
use rsbinder::*;

rsbinder::include_aidl!("hello", crate::hello::IHello::*);

struct HelloService;
impl Interface for HelloService {}
impl IHello for HelloService {
    fn echo(&self, message: &str) -> BinderResult<String> {
        Ok(format!("Echo: {message}"))
    }
}

// The service process: register with the service manager and serve.
rsbinder::serve(Uri::kernel())?
    .add("hello_service", BnHello::new_binder(HelloService))?
    .run()?;

// The client process: wait for the service and call it.
let hello: Strong<dyn IHello> = rsbinder::connect(Uri::kernel().with_service("hello_service"))?;
println!("{}", hello.echo("Hello, World!")?);
```

Over RPC, `Endpoint::unix("/tmp/hello.sock")` replaces `Uri::kernel()` on both
sides; nothing else changes. On Linux, kernel binder needs a binder device and
a service manager, which [`rsbinder-tools`](https://crates.io/crates/rsbinder-tools)
provides:

```bash
cargo install rsbinder-tools
sudo rsb_device binder --group "$(id -gn)" --mode 0660
rsb_hub --insecure-allow-all    # development only; see rsb_hub --help for a policy
```

## Android compatibility

rsbinder implements Android's protocols in Rust rather than binding to
`libbinder`: a peer receives the bytes a C++ or Java process would send.
Layer by layer:

- **Kernel driver protocol** — the same ioctl commands (`BC_*` / `BR_*`),
  `binder_transaction_data` and object types (binder, handle, fd) as
  `libbinder`. On a device rsbinder opens the existing `/dev/binder` and talks
  to the device's own service manager.
- **Parcel wire** — a value is written as the bytes `libbinder` writes for it,
  little-endian on every host.
- **AIDL** — `rsbinder-aidl` compiles the same `.aidl` a C++ or Java peer is
  built from and refuses what AOSP `aidl` refuses, so both sides agree on
  transaction codes and parcel layout. Stable AIDL's interface version and
  hash are supported.
- **Service manager** — each Android release's `IServiceManager` protocol,
  selected with the `android_*` features: Android 10's legacy C protocol, both
  Android 15 numberings (probed on the device), and Android 17 through
  `android_16`, its wire being identical. On Linux,
  [`rsb_hub`](https://crates.io/crates/rsbinder-tools) serves the same
  interface.
- **RPC transport** — the `RpcServer` / `RpcSession` wire of `libbinder`: the
  Android 12 profile, and the Android 13+ profile with wire versions 0–2
  (Android 13–16), verified against real `libbinder` on Android 13–16
  emulators.
- **Payloads beyond plain data** — `ParcelFileDescriptor`, `IMemory` shared
  memory and Fast Message Queue (`libfmq`'s ring layout) use the AOSP wire.

The features to enable per release, and the calls a release's service manager
lacks (Android 10's legacy protocol has no `isDeclared` and no service
notifications), are in the book's
[Android chapter](https://hiking90.github.io/rsbinder/android.html#android-version-compatibility).

## Feature flags

| Feature | Default | Enables |
| --- | :---: | --- |
| `tokio` | ✅ | async/await on Tokio (implies `async`) |
| `async` | | async interfaces without committing to a runtime |
| `macros` | | `#[rsbinder::interface]` and the `Parcelable` / `BinderEnum` / `ServiceSpecificError` derives: an interface as a Rust trait, no `.aidl` |
| `rpc` | | the RPC transport over Unix sockets |
| `rpc-tls` | | a TLS backend over rustls (implies `rpc`) |
| `rpc-vsock` | | a vsock backend, host ↔ VM, Linux and Android (implies `rpc`) |
| `rpc-tcp-debug` | | plaintext TCP, for debugging and interop bring-up only (implies `rpc`) |
| `tracing` | | `tracing` spans named as AOSP names its ATrace sections |
| `android_10` … `android_16`, `android_*_plus` | | the Android service-manager protocols to speak on a device; see [Android](https://hiking90.github.io/rsbinder/android.html) |

## Requirements

- Rust 1.86 or newer.
- Linux kernel binder: binderfs (`CONFIG_ANDROID_BINDERFS=y`); see
  [Enable binder for Linux](https://hiking90.github.io/rsbinder/enable-binder-for-linux.html).
- Android: API 29 or newer, built with [cargo-ndk](https://github.com/bbqsrc/cargo-ndk)
  (`cargo ndk -t aarch64-linux-android --platform 29 build`).
- macOS: the RPC transport only.

## Documentation

- [The rsbinder book](https://hiking90.github.io/rsbinder/): AIDL, services,
  shared memory, streaming, the RPC transport, Android compatibility.
- [API reference](https://docs.rs/rsbinder) and
  [stability tiers](https://hiking90.github.io/rsbinder/stability-tiers.html)
  (each public API is Stable, Provisional or Experimental).
- [Migrating from 0.12 to 0.13](https://hiking90.github.io/rsbinder/migrating-0.13.html).

Related crates, each with its README on GitHub:
[`rsbinder-aidl`](https://github.com/hiking90/rsbinder/tree/master/rsbinder-aidl),
[`rsbinder-macros`](https://github.com/hiking90/rsbinder/tree/master/rsbinder-macros),
[`rsbinder-fmq`](https://github.com/hiking90/rsbinder/tree/master/rsbinder-fmq),
[`rsbinder-topic`](https://github.com/hiking90/rsbinder/tree/master/rsbinder-topic),
[`rsbinder-tools`](https://github.com/hiking90/rsbinder/tree/master/rsbinder-tools).
The source, issues and release notes
([CHANGELOG](https://github.com/hiking90/rsbinder/blob/master/CHANGELOG.md))
are at [github.com/hiking90/rsbinder](https://github.com/hiking90/rsbinder).

Apache-2.0.

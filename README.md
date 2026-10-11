# rsbinder

**rsbinder** is a pure Rust implementation of Binder IPC for Linux, Android, and macOS.

[![crates.io](https://img.shields.io/crates/v/rsbinder.svg)](https://crates.io/crates/rsbinder)
[![Docs.rs](https://docs.rs/rsbinder/badge.svg)](https://docs.rs/rsbinder)
[![Rust Version](https://img.shields.io/badge/rustc-1.86+-blue.svg)](https://blog.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](https://github.com/hiking90/rsbinder/blob/master/LICENSE-APACHE)

It offers two transports behind the same AIDL interfaces:

* **Kernel binder** — the `/dev/binder` driver on Linux and Android, with the same protocol and wire format as Android `libbinder`, so rsbinder talks directly to existing Android services written in C++ or Java.
* **RPC transport** — binder over Unix, vsock, or TLS sockets, with no kernel driver and no root. Wire-compatible with Android `libbinder` RPC, verified against real `libbinder` on Android 13–16.

If you'd rather use C++ on Linux, see [binder-linux](https://github.com/hiking90/binder-linux).

## Crates

* **rsbinder** — the binder library: services, clients, parcels, async.
* **[rsbinder-aidl][rsbinder-aidl-readme]** — AIDL → Rust code generator.
* **[rsbinder-macros][rsbinder-macros-readme]** — `#[rsbinder::interface]`: declare an interface as a Rust trait, without `.aidl`.
* **[rsbinder-fmq][rsbinder-fmq-readme]** — Android's Fast Message Queue (`libfmq`), independent of binder.
* **[rsbinder-tools][rsbinder-tools-readme]** — `rsb_hub` (service manager for Linux), `rsb_device`, `rsb_service`.

[rsbinder-aidl-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-aidl/README.md
[rsbinder-macros-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-macros/README.md
[rsbinder-fmq-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-fmq/README.md
[rsbinder-tools-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/README.md

## Features

* Kernel binder and RPC transport, chosen at run time by a `Uri` (`rsbinder::serve`, `rsbinder::connect`).
* AIDL compiler compatible with Android's, and interface macros that generate the same code.
* Sync and Tokio async APIs.
* `ParcelFileDescriptor`, shared memory (`IMemory`), Fast Message Queue and streaming.
* Storing AIDL values to bytes (`to_bytes` / `from_bytes`).
* Service manager for Linux (`rsb_hub`) with access control and on-demand start.

## Platform Support

| Platform | Kernel binder | RPC transport |
|----------|:-------------:|:-------------:|
| Linux    | ✅ (binderfs)  | ✅ |
| Android  | ✅ (10–17, API 29–37) | ✅ |
| macOS    | —             | ✅ |

Parcel data is little-endian on every host, as on Android.

## Getting Started

**RPC transport** (no kernel config, no root):
```
$ cargo run -p example-hello --features rpc --bin rpc_hello_service
$ cargo run -p example-hello --features rpc --bin rpc_hello_client
```

**Kernel binder** needs binderfs (`CONFIG_ANDROID_BINDERFS=y`; Arch Linux `linux-zen` has it, for Ubuntu see [Enable Binder on Ubuntu](https://hiking90.github.io/rsbinder/ubuntu-linux.html)):
```
$ cargo build
$ sudo target/debug/rsb_device binder --group "$(id -gn)" --mode 0660
$ cargo run --bin rsb_hub -- --insecure-allow-all
$ cargo run --bin hello_service
$ cargo run --bin hello_client
```

**Android**: build with [cargo-ndk](https://github.com/bbqsrc/cargo-ndk) for API 29 or newer:
```
$ cargo ndk -t aarch64-linux-android --platform 29 build --release
```

## Documentation

The **[rsbinder book](https://hiking90.github.io/rsbinder/)** covers installation, AIDL, the RPC transport, Android compatibility and migration between releases. API reference is on [docs.rs](https://docs.rs/rsbinder).

rsbinder is pre-1.0. Each public API is classified Stable, Provisional or Experimental; see [Stability Tiers](https://hiking90.github.io/rsbinder/stability-tiers.html).

## Contributing

See [CONTRIBUTING.md](https://github.com/hiking90/rsbinder/blob/master/CONTRIBUTING.md).

## License

Licensed under the Apache License, Version 2.0.

## Notice

Many source files in **rsbinder** were developed by quoting or referencing Android's binder implementation.

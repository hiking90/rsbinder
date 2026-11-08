# rsbinder

**rsbinder** is a pure Rust implementation of Android's Binder IPC for Linux, Android, and macOS. **It is compatible with Android at the protocol level**: an rsbinder client calls the C++ and Java services of an Android 10–17 device, and an rsbinder service answers their clients, with no `libbinder` linked.

[![crates.io](https://img.shields.io/crates/v/rsbinder.svg)](https://crates.io/crates/rsbinder)
[![Docs.rs](https://docs.rs/rsbinder/badge.svg)](https://docs.rs/rsbinder)
[![Rust Version](https://img.shields.io/badge/rustc-1.86+-blue.svg)](https://blog.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE-APACHE)

It offers two transports behind the same AIDL interfaces:

* **Kernel binder** — the `/dev/binder` driver on Android, and on Linux through binderfs.
* **RPC transport** — binder over Unix, vsock, or TLS sockets, with no kernel driver and no root.

Compatibility holds at each layer: the kernel driver protocol, the parcel wire, AIDL, each release's service manager, and `libbinder`'s RPC wire. [Android compatibility](rsbinder/README.md#android-compatibility) describes each.

## Crates

| Crate | Version | |
| --- | --- | --- |
| [rsbinder](rsbinder/README.md) | [![](https://img.shields.io/crates/v/rsbinder.svg)](https://crates.io/crates/rsbinder) | The binder library: services, clients, parcels, async, the RPC transport |
| [rsbinder-aidl](rsbinder-aidl/README.md) | [![](https://img.shields.io/crates/v/rsbinder-aidl.svg)](https://crates.io/crates/rsbinder-aidl) | AIDL → Rust code generator |
| [rsbinder-macros](rsbinder-macros/README.md) | [![](https://img.shields.io/crates/v/rsbinder-macros.svg)](https://crates.io/crates/rsbinder-macros) | `#[rsbinder::interface]`: an interface as a Rust trait, without `.aidl` |
| [rsbinder-fmq](rsbinder-fmq/README.md) | [![](https://img.shields.io/crates/v/rsbinder-fmq.svg)](https://crates.io/crates/rsbinder-fmq) | Android's Fast Message Queue (`libfmq`) and the shared-memory primitives under it |
| [rsbinder-topic](rsbinder-topic/README.md) | [![](https://img.shields.io/crates/v/rsbinder-topic.svg)](https://crates.io/crates/rsbinder-topic) | One publisher, many subscribers, records in shared memory (experimental) |
| [rsbinder-tools](rsbinder-tools/README.md) | [![](https://img.shields.io/crates/v/rsbinder-tools.svg)](https://crates.io/crates/rsbinder-tools) | `rsb_hub` (service manager for Linux), `rsb_device`, `rsb_service` |

## Platform Support

| Platform | Kernel binder | RPC transport |
|----------|:-------------:|:-------------:|
| Linux    | ✅ (binderfs)  | ✅ |
| Android  | ✅ (10–17, API 29–37) | ✅ |
| macOS    | —             | ✅ |

## Quick start

The RPC transport needs no kernel configuration and no root:
```
$ cargo run -p example-hello --features rpc --bin rpc_hello_service
$ cargo run -p example-hello --features rpc --bin rpc_hello_client
```

Kernel binder needs binderfs and a service manager; see [Getting Started](https://hiking90.github.io/rsbinder/getting-started.html).

## Documentation

The **[rsbinder book](https://hiking90.github.io/rsbinder/)** covers installation, AIDL, the RPC transport, Android compatibility and migration between releases; the API reference is on [docs.rs](https://docs.rs/rsbinder). rsbinder is pre-1.0, and each public API is classified Stable, Provisional or Experimental ([Stability Tiers](https://hiking90.github.io/rsbinder/stability-tiers.html)). Release notes are in [CHANGELOG.md](CHANGELOG.md), and contributing in [CONTRIBUTING.md](CONTRIBUTING.md).

For C++ on Linux, see [binder-linux](https://github.com/hiking90/binder-linux).

## License

Licensed under the Apache License, Version 2.0. Many source files in **rsbinder** were developed by quoting or referencing Android's binder implementation.

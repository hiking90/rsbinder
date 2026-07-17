# rsbinder

**rsbinder** provides crates implemented in pure Rust that make Binder IPC available on Linux, Android, and macOS.

[![crates.io](https://img.shields.io/crates/v/rsbinder.svg)](https://crates.io/crates/rsbinder)
[![Docs.rs](https://docs.rs/rsbinder/badge.svg)](https://docs.rs/rsbinder)
[![Rust Version](https://img.shields.io/badge/rustc-1.85+-blue.svg)](https://blog.rust-lang.org/)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](LICENSE)

## Why rsbinder

Android's Binder IPC mechanism has been in the mainline Linux kernel since 2015, but adoption outside Android has been limited by the lack of Rust-native tooling. **rsbinder** fills that gap with two complementary transports:

* **Kernel binder** — the `/dev/binder` driver on Linux and Android. Same protocol and wire format as Android `libbinder`, so an AIDL-generated rsbinder client can call existing Android services written in C++ or Java directly — see [Android Development](book/src/android.md#protocol-compatibility).
* **RPC transport (binder-over-socket)** — a separate stack that works on Linux, macOS, and Android without needing the kernel driver or root. Wire-compatible with Android `libbinder` RPC v1 and v2, verified in both directions against real `libbinder` on Android 13–16.

For Android developers writing system-level Rust, **rsbinder** is the missing NDK-level binder API. For Linux and macOS, it brings binder-style IPC to environments where it was previously impractical. If you'd rather use C++ on Linux, see [binder-linux](https://github.com/hiking90/binder-linux).

## Overview

* **crate rsbinder** — library for implementing binder service / client functionality.
* **[crate rsbinder-aidl][rsbinder-aidl-readme]** — AIDL → Rust code generator.
* **[crate rsbinder-tools][rsbinder-tools-readme]** — CLI tools, including the Binder Service Manager for Linux (`rsb_hub`).
* **[crate tests][tests-readme]** — Android binder test cases ported to rsbinder.
* **[crate example-hello][example-hello-readme]** — example service / client written using rsbinder.

[rsbinder-aidl-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-aidl/README.md
[rsbinder-tools-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/README.md
[tests-readme]: https://github.com/hiking90/rsbinder/blob/master/tests/README.md
[example-hello-readme]: https://github.com/hiking90/rsbinder/tree/master/example-hello/README.md

## Documentation

For a comprehensive guide — architecture, installation, tutorials — see the **[Rsbinder Development Guide](https://hiking90.github.io/rsbinder/)**.

The book source lives in [`book/`](book/) and can be built locally with [mdBook](https://github.com/rust-lang/mdBook):
```
$ cd book
$ mdbook serve
```

## Current Development Status

**rsbinder** is pre-1.0 — the API may still change before 1.0. Core binder, AIDL, RPC transport, and Android `libbinder` interop are exercised by CI across Android API 29–36 and a Linux native-kernel-binder host; Android 17 (API 37) is validated on an emulator.

## Platform Support

| Platform | Kernel binder (`/dev/binder`) | RPC transport (binder-over-socket) |
|----------|:-----------------------------:|:----------------------------------:|
| Linux    | ✅ (binderfs)                  | ✅                                  |
| Android  | ✅ (API 29–37)                 | ✅ (`libbinder` RPC v1 / v2 interop) |
| macOS    | —                             | ✅ (first-class)                    |

The RPC transport requires no kernel module, no root, and no special device file — making rsbinder usable as a general cross-platform Rust IPC layer in addition to its Android role.

## RPC Transport (binder-over-socket)

A separate stack from the kernel binder path. Lets you run binder-style IPC **without `/dev/binder`** — on Linux, macOS, or Android, and across host/VM or network boundaries. Wire-compatible with Android `libbinder` RPC v1 and v2, verified end-to-end against real Android 15 / 16 emulators.

Disabled by default; zero-cost when off. Opt in with cargo features:

| Feature          | Purpose                                                       |
|------------------|---------------------------------------------------------------|
| `rpc`            | Master switch; enables Unix-socket transport.                 |
| `rpc-tcp-debug`  | Plain TCP — **bring-up / interop only**, not production.      |
| `rpc-vsock`      | host↔VM (Android Virtualization Framework / Microdroid).      |
| `rpc-tls`        | TLS over rustls for untrusted networks.                       |

Capabilities: FD-over-RPC (`ParcelFileDescriptor`), death notification (session disconnect, AOSP-faithful), Tokio async adapter, multi-connection, `IAccessor` client / server, and `rsb_hub` `addService` accessor auto-detect. See [`book/src/rpc-transport.md`](book/src/rpc-transport.md).

### Stability tiers

rsbinder uses a 3-tier model on the path to 1.0 — **Stable** (semver-strict, AOSP-faithful), **Provisional** (signature may tweak in a minor bump; wire format already locked), **Experimental** (opt-in Cargo feature, wire format may change). See [`book/src/stability-tiers.md`](book/src/stability-tiers.md) for the per-API mapping. PRs run `cargo-semver-checks` for both `rsbinder` and `rsbinder-aidl`, so a breaking change to any Stable / Provisional surface is visible on the PR before merge.

Quick try (no kernel config or root):
```
$ cargo run -p example-hello --features rpc --bin rpc_hello_service
$ cargo run -p example-hello --features rpc --bin rpc_hello_client
```

## Cross-transport services & authorization

Write service registration and lookup **once** and pick kernel binder or RPC by construction — the `rsbinder::service` facade (`Registry` / `Broker`) keeps the AIDL interface, generated stubs, and call sites transport-agnostic. Async works the same way over either transport. Calling identity and authorization stay coherent across the trust boundary: `get_calling_uid()` returns the kernel-vouched peer uid over Unix RPC, and `@EnforcePermission` methods fail closed (deny) over RPC rather than silently granting.

See [Cross-Transport Services](book/src/cross-transport-services.md), [Security & Authorization](book/src/security.md), and [Async Service](book/src/async-service.md) in the book.

## Prerequisites to build and test

There are two transport paths. Pick whichever fits your environment.

### Path A — RPC transport (no kernel binder)

Works on Linux, macOS, and Android with no special kernel config or root. See the [RPC Transport](#rpc-transport-binder-over-socket) section above for the example commands and backend options.

### Path B — Kernel binder (Linux / Android)

Enable binderfs in the Linux kernel:
```
CONFIG_ANDROID=y
CONFIG_ANDROID_BINDER_IPC=y
CONFIG_ANDROID_BINDERFS=y
```
* **Arch Linux** — `linux-zen` already includes BinderFS: `pacman -S linux-zen`
* **Ubuntu** — see https://github.com/anbox/anbox/blob/master/docs/install.md

Build, bring up the service manager, then run the example:
```
$ cargo build
$ sudo target/debug/rsb_device binder    # create /dev/binder
$ cargo run --bin rsb_hub                # service manager
$ cargo run --bin hello_service
$ cargo run --bin hello_client
```
`rsb_device` and `rsb_hub` are documented under [`rsbinder-tools`][rsbinder-tools-readme].

### Cross compile to Android device
Please follow the [cargo-ndk](https://github.com/bbqsrc/cargo-ndk) guide.

## Compatibility Goal with Android Binder

### Mutual Communication
**rsbinder** and Android Binder share the same wire protocol, so Android services and rsbinder clients (and vice versa) interoperate directly. End-to-end interop is verified against real Android `libbinder` for both the kernel binder path and the RPC transport.

### Protocol Level Compatibility
**rsbinder** implements the same low-level Binder protocol as Android, ensuring binary compatibility at the kernel interface level:
- **Transaction Format** — identical `binder_transaction_data` structures.
- **Object Types** — all Android Binder object types (BINDER, HANDLE, FD).
- **Command Protocols** — same ioctl commands (BC_* / BR_* protocol).
- **Memory Management** — compatible parcel serialization and shared-memory handling.

### Android Version Support
**rsbinder** supports Android versions 10 through 17 (API levels 29–37). Android 15 (SDK 35) and Android 17 (SDK 37) share the Android 14 / Android 16 service-manager wire format respectively, so they are covered by the `android_14` / `android_16` features — no dedicated feature flag is needed. Android 10 uses the legacy C service manager protocol; APIs not implemented there (`is_declared`, `register_for_notifications`, `unregister_for_notifications`, `get_service_debug_info`) return an error or `false` so callers can detect the gap. CI exercises emulator API levels 29, 30, 32, 34, and 36.

### AIDL Compatibility
The **rsbinder-aidl** compiler generates Rust code that maintains compatibility with Android's AIDL:
- **Interface Definition** — same `.aidl` syntax and semantics.
- **Data Types** — all AIDL primitive and complex types.
- **Parcelable** — compatible serialization with Android's Parcelable.

### RPC Wire Compatibility
Wire-compatible with Android `libbinder` RPC v1 and v2 — verified end-to-end against real Android 15 (v1 native) and Android 16 (v2) `libbinder` for transactions, FD passing, and `IAccessor` bridging.

### VINTF Accessor
`rsb_hub` auto-detects the `android.os.IAccessor` descriptor on `addService` and serves it through `getService` / `checkService`, matching Android's `<accessor>` entry semantics.

### API Differences
Complete API parity is not a goal — rsbinder's architecture differs from `libbinder` in places that matter for Rust idiom (ownership, async, error handling). The semantics that affect wire and observable behavior are matched; the surface API is not a literal port.

## Todo

**Core**
- [x] Binder crate.
- [x] AIDL compiler (with enhanced error diagnostics).
- [x] `ParcelFileDescriptor`.
- [x] Ported Android `test_service` / `test_client`.
- [x] Tokio async support.
- [x] Removed all `todo!()` / `unimplemented!()` macros.
- [x] Compatibility testing with Binder on Android.

**RPC transport**
- [x] RPC transport (binder-over-socket).
- [x] macOS support (RPC).
- [x] FD-over-RPC.
- [x] `IAccessor` client / server.
- [x] Real Android `libbinder` interop (RPC v1 / v2).

**Tooling**
- [ ] (In Progress) Service Manager (**rsb_hub**) for Linux — lazy-service poller and accessor descriptor auto-detect done.

## Contributing

See [CONTRIBUTING.md](CONTRIBUTING.md) for the build / test workflow,
PR checklist, and the project's comment & docstring policy (which
exempts public API rustdoc from the "one short line max" rule).

## License
**rsbinder** is licensed under the **Apache License version 2.0**.

## Notice
Many of the source files in **rsbinder** have been developed by quoting or referencing Android's binder implementation.

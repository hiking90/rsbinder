# Overview
Welcome to **rsbinder**!

**rsbinder** is a Rust library and toolset that enables you to utilize Binder IPC on Linux and Android OS. It provides pure Rust implementations that make Binder IPC available across both platforms.

Binder IPC is an object-oriented IPC (Inter-Process Communication) mechanism that Google added to the Linux kernel for Android. Android uses Binder IPC for all process communication, and it has been part of the mainline Linux kernel since version 4.17, making it available on all modern Linux systems.

However, since it is rarely used outside of Android, it is disabled by default in most Linux distributions.

**rsbinder ships two parallel stacks:**

1. The traditional **kernel binder** path through `/dev/binder` /
   `/dev/binderfs/binder`, which most of this guide covers.
2. An **RPC transport** (binder-over-socket) — AOSP's
   `RpcServer`/`RpcSession` analogue, pure user-space, no kernel
   driver. Drives the same generated AIDL stubs over Unix-domain
   sockets, vsock, or TLS, and runs on Linux, Android, **and macOS**.
   See [RPC Transport](./rpc-transport.md).

## Crates

**rsbinder** offers the following crates:

* **`rsbinder`**: Core library crate for implementing binder service/client functionality.
* **`rsbinder-aidl`**: AIDL-to-Rust code generator for rsbinder.
* **`rsbinder-tools`**: CLI tools, including a Binder Service Manager (`rsb_hub`) for Linux.
* **`tests`**: Port of Android's binder test cases for client/server testing.
* **`example-hello`**: Example service/client implementation using rsbinder.

## Key Features of Binder IPC

- **Object-oriented**: Binder IPC provides a clean and intuitive object-oriented API for inter-process communication.
- **Efficient**: Binder IPC is designed for high performance and low overhead with efficient data serialization.
- **Secure**: Binder IPC provides strong security features to prevent unauthorized access and tampering.
- **Versatile**: Binder IPC can be used for a variety of purposes, including remote procedure calls, data sharing, and event notification.
- **Cross-platform**: Works on Linux and Android (kernel binder), plus macOS (RPC transport only).
- **Async/Sync Support**: Supports both synchronous and asynchronous programming models with optional tokio runtime integration.
- **Two transport stacks**: Kernel binder for on-device IPC, and an opt-in RPC transport (Unix sockets, vsock, or TLS) for cross-process, cross-VM, or cross-host binder calls — both stacks share the same AIDL-generated code.

## Core Components

- **Parcel**: Data serialization and deserialization for IPC transactions
- **Binder Objects**: Strong and weak references for cross-process communication
- **AIDL Compiler**: Generates Rust code from Android Interface Definition Language files
- **Service Manager**: Centralized service discovery and registration (rsb_hub for Linux)
- **Thread Pool**: Efficient handling of concurrent IPC transactions
- **Death Notification**: Service lifecycle management and cleanup

## Resources

- **API Documentation**: [docs.rs/rsbinder](https://docs.rs/rsbinder)
- **Repository**: [github.com/hiking90/rsbinder](https://github.com/hiking90/rsbinder)

Before using Binder IPC on Linux, you must enable the feature in the kernel. Please refer to [Enable binder for Linux](./enable-binder-for-linux.md) for detailed instructions. For Android, see [Android Development](./android.md).

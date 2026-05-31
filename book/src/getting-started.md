# Getting Started

Welcome to **rsbinder**! This guide will help you get started with Binder IPC development using Rust.

## Learning Path

If you are new to Binder IPC, we recommend following this learning path:

1. **[Overview](./overview.md)** and **[Architecture](./architecture.md)** - Start here to understand Binder IPC fundamentals
   - Learn about the core concepts and components
   - Understand the relationship between services and clients
   - See how AIDL generates Rust code

2. **[Installation](./installation.md)** - Set up your development environment
   - Install required dependencies
   - Set up binder devices and service manager
   - Configure your Rust project

3. **[Hello World](./hello-world.md)** - Build your first Binder service
   - Create a simple echo service
   - Learn AIDL basics
   - Understand service registration and client communication

4. **AIDL Guide** - Dive deeper into AIDL language features:
   - **[Data Types](./aidl-data-types.md)** - How AIDL types map to Rust types
   - **[Parcelable](./aidl-parcelable.md)** - Custom data structures for IPC
   - **[Enum and Union](./aidl-enum-union.md)** - Enum and union type support
   - **[Annotations](./aidl-annotations.md)** - Code generation annotations

5. **Service Development** - Build production-quality services:
   - **[Service Patterns](./service-patterns.md)** - Advanced service patterns and best practices
   - **[Async Service](./async-service.md)** - Non-blocking services with tokio
   - **[Callbacks and Interfaces](./callbacks-and-interfaces.md)** - Bidirectional communication
   - **[ParcelFileDescriptor](./parcel-file-descriptor.md)** - File descriptor passing
   - **[Error Handling](./error-handling.md)** - Error types and handling strategies
   - **[Service Manager (HUB)](./service-manager.md)** - Service registration and discovery

6. **[RPC Transport](./rpc-transport.md)** - Binder-over-socket:
   - The opt-in second stack that runs on Linux, Android, and macOS
   - Unix-domain sockets, vsock, or TLS instead of `/dev/binder`
   - Same generated AIDL stubs as the kernel-binder path

7. **Platform-specific Setup** - Choose your target platform:
   - **[Linux Setup](./enable-binder-for-linux.md)** - For Linux development
   - **[Android Development](./android.md)** - For Android integration

## Platform Requirements

**rsbinder** ships two parallel stacks — pick by platform and use case:

- **Kernel binder** (the default in this guide): Linux 5.0+ with
  binderfs enabled (the `binderfs` filesystem landed in 5.0), or
  Android. Talks to the kernel binder driver through
  `/dev/binderfs/binder` (Linux) or `/dev/binder` (Android).
- **RPC transport** (binder-over-socket, opt-in via the `rpc`
  feature): pure user-space, no kernel binder driver. Runs on
  **Linux, Android, and macOS** over Unix-domain sockets, vsock, or
  TLS. See the [RPC Transport](./rpc-transport.md) chapter.

> **Windows**: not supported on either stack.
>
> **macOS**: kernel binder is not supported (no kernel driver), but
> the RPC transport works natively — useful for developing and
> testing RPC services on a macOS workstation without a Linux VM.

## Quick Start Checklist

Before diving into development, ensure you have:

- [ ] Rust 1.85+ installed
- [ ] Linux kernel with binder support enabled (or an Android device/emulator)
- [ ] Created binder device using `rsb_device` (Linux only)
- [ ] Service manager (`rsb_hub`) running (Linux only)
- [ ] Basic understanding of AIDL syntax (covered in the [Hello World](./hello-world.md) tutorial)

## Key Concepts to Understand

- **Services**: Server-side implementations that provide functionality
- **Clients**: Applications that consume services through proxies
- **AIDL**: Interface definition language for describing service contracts
- **Service Manager**: Central registry for service discovery
- **Parcels**: Serialization format for data exchange
- **Binder Objects**: References that enable cross-process communication

## Common Development Workflow

1. Define your service interface in an `.aidl` file
2. Use `rsbinder-aidl` to generate Rust code
3. Implement your service logic
4. Build a `kernel::Host` (via its builder) and register the service with it
5. Create clients that discover the service through a `kernel::Broker`

Steps 4–5 use the `rsbinder::service` host/broker facade — the recommended entry point,
which also lets you switch to the [RPC transport](./rpc-transport.md) by changing one line.
See [Service Patterns](./service-patterns.md) and [Cross-Transport Services](./cross-transport-services.md).

Ready to start? Head to the [Overview](./overview.md) section to learn the fundamentals!

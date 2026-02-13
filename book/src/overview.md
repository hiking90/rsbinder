# Overview
Welcome to **rsbinder**!

**rsbinder** is a Rust library and toolset that enables you to utilize Binder IPC on Linux and Android OS. It provides pure Rust implementations that make Binder IPC available across both platforms.

Binder IPC is an object-oriented IPC (Inter-Process Communication) mechanism that Google added to the Linux kernel for Android. Android uses Binder IPC for all process communication, and since 2015, it has been integrated into the Linux kernel, making it available on all Linux systems.

However, since it is rarely used outside of Android, it is disabled by default in most Linux distributions.

## Core Components

**rsbinder** offers the following features:

* **crate rsbinder**: A library crate for implementing binder service/client functionality.
* **crate rsbinder-aidl**: A tool for generating Rust code for rsbinder from AIDL files.
* **crate rsbinder-tools**: Provides CLI tools, including a Binder Service Manager for Linux.
* **crate tests**: Port of Android's binder test cases to provide various client/server testing features.
* **crate example-hello**: An example of service/client written using rsbinder.

### Key Features of Binder IPC:

- **Object-oriented**: Binder IPC provides a clean and intuitive object-oriented API for inter-process communication.
- **Efficient**: Binder IPC is designed for high performance and low overhead with efficient data serialization.
- **Secure**: Binder IPC provides strong security features to prevent unauthorized access and tampering.
- **Versatile**: Binder IPC can be used for a variety of purposes, including remote procedure calls, data sharing, and event notification.
- **Cross-platform**: Works on both Android and Linux environments.

### Core Components:

- **Parcel**: Data serialization and deserialization for IPC transactions
- **Binder Objects**: Strong and weak references for cross-process communication
- **AIDL Compiler**: Generates Rust code from Android Interface Definition Language files
- **Service Manager**: Centralized service discovery and registration (rsb_hub for Linux)
- **Thread Pool**: Efficient handling of concurrent IPC transactions
- **Death Notification**: Service lifecycle management and cleanup

Before using Binder IPC, you must enable the feature in the kernel. Please refer to [Enable binder for Linux](./enable-binder-for-linux.md) for detailed instructions on setting it up.

# rsbinder

**rsbinder** provides crates implemented in pure Rust that make Binder IPC available on both Android and Linux.

[![Rust](https://github.com/hiking90/rsbinder/actions/workflows/build.yml/badge.svg)](https://github.com/hiking90/rsbinder/actions/workflows/build.yml)

## Binder IPC: Available on Linux, Untapped Potential

While Android's Binder IPC mechanism was merged into the Linux kernel back in 2015, its adoption within the broader Linux ecosystem remains limited. This project aims to address that by providing libraries and tools specifically designed for using Binder IPC in Linux environments.

One key reason for limited adoption is the lack of readily available tools and libraries optimized for the Linux world. This project tackles that challenge by leveraging Rust's strengths for efficient thread utilization, a crucial aspect for maximizing Binder IPC performance on Linux.

However, this project focuses on pure Rust implementations. If you're interested in C++-based Binder IPC for Linux, consider checking out the [binder-linux](https://github.com/hiking90/binder-linux) project.

Although this project focuses on supporting Binder IPC in the Linux environment, it also provides compatibility with Android's Binder IPC. [Compatibility Goal with Android Binder](#Compatibility-Goal-with-Android-Binder)

## Current Development Status
**rsbinder** is still in its early development stages and is not yet ready for product development.

## Overview
**rsbinder** offers the following features:

* **crate rsbinder**: A library crate for implementing binder service/client functionality.
* **[crate rsbinder-aidl][rsbinder-aidl-readme]**: A tool for generating Rust code for rsbinder from aidl.
* **[crate rsbinder-tools][rsbinder-tools-readme]**: Provides CLI tools for Linux.
* **[crate rsbinder-tests][rsbinder-tests-readme]**: Provides functionality similar to Binder's ServiceManager.
* **[crate example-hello][example-hello-readme]**: An example of service/client written using rsbinder.

[rsbinder-aidl-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-aidl/README.md
[rsbinder-tools-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/README.md
[rsbinder-tests-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tests/README.md
[example-hello-readme]: https://github.com/hiking90/rsbinder/tree/master/example-hello/README.md

## Prerequisites to build and test

### Enable binder for Linux
* The Linux kernel must be built with support for binderfs. Please check the following kernel configs.
```
CONFIG_ASHMEM=y
CONFIG_ANDROID=y
CONFIG_ANDROID_BINDER_IPC=y
CONFIG_ANDROID_BINDERFS=y
```

* Arch Linux - Install linux-zen kernel. Zen kernel already includes BinderFS.
```
$ pacman -S linux-zen
```
* Ubuntu Linux - https://github.com/anbox/anbox/blob/master/docs/install.md

### Build rsbinder
Build all rsbinder crates.
```
$ cargo build
```

#### Run rsbinder tools
* Run **[rsb_device]** command to create a binder device file.
```
$ sudo target/debug/rsb_device binder
```
[rsb_device]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/README.md
* Run **[rsb_hub]**. It is a binder service manager.
```
$ cargo run --bin rsb_hub
```
[rsb_hub]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-tools/README.md

### Test binder for Linux
* Run **hello_service**
```
$ cargo run --bin hello_service
```
* Run **hello_client**
```
$ cargo run --bin hello_client
```

### Cross compile to Android device
* Please follow the guideline of https://github.com/bbqsrc/cargo-ndk

## Compatibility Goal with Android Binder
### Mutual Communication:
Both rsbinder and Android Binder utilize the same core protocol, enabling seamless communication between Android services and rsbinder clients, and vice versa. However, continued development is currently underway to further refine this interoperability.

### API Differences:
Complete API parity between rsbinder and Android Binder isn't available due to fundamental differences in their underlying architectures. Nonetheless, both APIs share a high degree of similarity, minimizing the learning curve for developers familiar with either system.

## Todo
- [x] Implement Binder crate.
- [x] Implement AIDL compiler.
- [x] Implement ParcelFileDescriptor.
- [x] Port Android test_service and test_client and pass the test cases.
- [x] Support Tokio async.
- [ ] (In Progress) Implement Service Manager(**rsb_hub**) for Linux
- [ ] (In Progress) Remove all todo!() and unimplemented!() macros.
- [ ] (In Progress) Performed compatibility testing with Binder on Android.
- [ ] Enhance error detection in AIDL code generator
- [ ] Support AIDL version and hash.

## License
**rsbinder** is licensed under the **Apache License version 2.0**.

## Notice
Many of the source codes in **rsbinder** have been developed by quoting or referencing Android's binder implementation.

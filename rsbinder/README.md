# rsbinder
**rsbinder** offers libraries and tools, purely implemented in Rust, for utilizing Binder IPC on Android and Linux.

Despite the integration of Android's binder IPC into the Linux kernel in 2015, the binder IPC is not being used at all in the Linux environment. 
The reason is believed to be the lack of libraries and tools operating in the Linux environment, and this has motivated the initiation of this project.

To maximize the performance of Binder IPC, it is essential to actively utilize threads, and this is why the project was started using Rust.

If you wish to use C++ based binder IPC in a Linux environment, please refer to the **[binder-linux]** project.

[binder-linux]: https://github.com/hiking90/binder-linux

## Current Development Status
**rsbinder** is still in its early development stages and is not yet ready for product development.
The source code still contains todo!() and unimplemented!().

## Overview
**rsbinder** offers the following features:

* **crate rsbinder**: A library crate for implementing binder service/client functionality.
* **[crate rsbinder-aidl][rsbinder-aidl-readme]**: A tool for generating Rust code for rsbinder from aidl.
* **[crate rsbinder-hub][rsbinder-hub-readme]**: Provides functionality similar to Binder's ServiceManager.
* **[crate rsbinder-tests][rsbinder-tests-readme]**: Provides functionality similar to Binder's ServiceManager.
* **[crate example-hello][example-hello-readme]**: An example of service/client written using rsbinder.

[rsbinder-aidl-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-aidl/README.md
[rsbinder-hub-readme]: https://github.com/hiking90/rsbinder/blob/master/rsbinder-hub/README.md
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
[rsb_device]: https://github.com/hiking90/rsbinder/blob/master/rsbinder/src/bin/rsb_device.md
* Run **rsb_hub**. It is a binder service manager.
```
$ cargo run --bin rsb_hub
```

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
* The Binder protocol is mutually compatible. That is, communication between an Android service and an rsbinder client is possible, and vice versa. However, this compatibility work is still ongoing.
* API compatibility is not provided. Android binder and rsbinder have different operating architectures and cannot offer the same APIs. However, there is a high similarity in APIs.

## Todo
- [x] Implement Binder crate.
- [x] Implement AIDL compiler.
- [x] Implement ParcelFileDescriptor.
- [x] Port Android test_service and test_client and pass the test cases.
- [ ] (In Progress) Implement Service Manager(**rsb_hub**) for Linux
- [ ] Remove all todo!() and unimplemented!() macros.
- [ ] Performed compatibility testing with Binder on Android.
- [ ] Support Tokio async.
- [ ] Enhance error detection in AIDL code generator
- [ ] Support Mandatory Access Control likes selinux and AppArmor.
- [ ] Support AIDL version and hash.

## License
**rsbinder** is licensed under the **Apache License version 2.0**.

## Notice
Many of the source codes in **rsbinder** have been developed by quoting or referencing Android's binder implementation.

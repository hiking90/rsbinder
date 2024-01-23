# rsbinder
**rsbinder** is a tool and library for utilizing Android's binder IPC, implemented purely in Rust language.

Despite its integration into the Linux kernel in 2015, Android's binder IPC has not been fully utilized in the Linux environment. This shortfall is often attributed to the lack of sufficient libraries and tools available for Linux, which inspired the inception of the **rsbinder** project.

## Current Development Status
**rsbinder** is still in its early development stages and is not yet ready for product development.
The source code still contains many todo!() macros, and the release of version 0.1 is planned only after all these todo!() macros are resolved.

## Overview
**rsbinder** offers the following features:

* **crate rsbinder**: A library crate for implementing binder service/client functionality.
* **crate rsbinder-aidl**: A tool for generating Rust code for rsbinder from aidl.
* **crate rsbinder-hub**: Provides functionality similar to Binder's ServiceManager.
* **crate rsbinder-tools**: Provides command line tools likes service manager and binder device initializor.
* **crate example-hello**: An example of service/client written using rsbinder.

## Prerequisites to build and test

### Android Build
* Please follow the guideline of https://github.com/bbqsrc/cargo-ndk

### Enable binder for Linux
* The Linux kernel must be built with support for binderfs. Please check the following kernel configs.
```
CONFIG_ASHMEM=y
CONFIG_ANDROID=y
CONFIG_ANDROID_BINDER_IPC=y
CONFIG_ANDROID_BINDERFS=y
```

* Arch Linux users just use the linux-zen kernel. Zen kernel already includes BinderFS.
* Ubuntu Linux users refer to https://github.com/anbox/anbox/blob/master/docs/install.md

#### Run rsbinder tools
* Build **rsbinder** crates. It can build **rsb_device** and it can be used to create a new binder device file.
```
$ sudo target/debug/rsb_device binder
```
* Run **rsb_hub**. It is a binder service manager.
```
$ target/debug/rsb_hub
```

### Test binder for Linux
* Run **hello_service**
```
$ target/debug/hello_service
```
* Run **hello_client**
```
$ target/debug/hello_client
```

## Compatibility Goal with Android Binder
* The Binder protocol is mutually compatible. That is, communication between an Android service and an rsbinder client is possible, and vice versa. However, this compatibility work is still ongoing.
* API compatibility is not provided. Android binder and rsbinder have different operating architectures and cannot offer the same APIs. However, there is a high similarity in APIs.

## Todo
- [x] Implement Binder crate.
- [x] Implement AIDL code generator.
- [x] Port Android test_service and test_client and pass the test cases.
- [x] Implement ParcelFileDescriptor.
- [ ] (In Progress) Implement Service Manager(**rsb_hub**) for Linux
- [ ] Remove all todo!() and unimplemented!() macros.
- [ ] Support Tokio async.
- [ ] Add more test cases for Binder IPC
- [ ] Enhance error detection in AIDL code generator
- [ ] Support MAC likes selinux and AppArmor.
- [ ] Support AIDL version and hash.

## License
**rsbinder** is licensed under the **Apache License version 2.0**.

## Notice
Many of the source codes in **rsbinder** have been developed by quoting or referencing Android's binder implementation.
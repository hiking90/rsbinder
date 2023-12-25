# rsbinder
**rsbinder** is a tool and library for utilizing Android's binder IPC, implemented purely in Rust language.

Android's binder IPC has been integrated into the Linux kernel in 2015. However, Android's binder IPC is not widely used in Linux.

This is thought to stem from the insufficient availability of libraries and tools for Linux, prompting the launch of the **rsbinder** project.

## Status
**rsbinder** is still in its early development stages and is not yet ready for product development.
The source code still contains many todo!() macros, and the release of version 0.1 is planned only after all these todo!() macros are resolved.

## Overview
**rsbinder** offers the following features:

* **rsbinder crate**: A library crate for implementing binder service/client functionality.
* **rsbinder-aidl crate**: A tool for generating Rust code for rsbinder from aidl.
* **rsbinder-hub crate**: Provides functionality similar to Binder's ServiceManager.
* **example-hello crate**: An example of service/client written using rsbinder.

## Todo
- [ ] Remove all todo!() macros.
- [ ] Implement Service Manager for Linux
- [ ] Add more test cases for Binder IPC
- [ ] Enhance error detection in AIDL code generator
- [ ] Support MAC likes selinux and AppArmor.

## License
**rsbinder** is licensed under the **Apache License version 2.0**.

## Notice
Many of the source codes in **rsbinder** have been developed by quoting or referencing Android's code.
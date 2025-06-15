# rsbinder-tools

This crate provides essential CLI tools for using Binder IPC on Linux systems. While Android has several built-in tools for binder IPC, Linux environments require additional utilities to set up and manage Binder IPC infrastructure.

## Installation

### From crates.io
```bash
$ cargo install rsbinder-tools
```

### From source
```bash
$ git clone https://github.com/hiking90/rsbinder.git
$ cd rsbinder
$ cargo build --release
```

## rsb_device

A utility for initializing the Linux binder environment and creating binder device files.

### Usage
```bash
$ sudo rsb_device <device_name>
```

### Example
```bash
$ sudo rsb_device binder
$ sudo rsb_device test_device
```

### What it does
**rsb_device** uses the kernel's binderfs feature to create new binder device files and requires root privileges. It performs the following operations:

1. **Directory Creation**: Creates `/dev/binderfs` directory if it doesn't exist
2. **Filesystem Mount**: Executes `mount -t binder binder /dev/binderfs` to mount binderfs
3. **Device Creation**: Uses kernel ioctl interface to create `/dev/binderfs/<device_name>`
4. **Permission Setup**: Sets permissions to 0666 for universal read/write access

### Output
After successful execution, the binder device will be accessible at `/dev/binderfs/<device_name>` and ready for IPC operations.

For detailed technical information, refer to the [Linux kernel binderfs documentation][kernel_binder_doc].

[kernel_binder_doc]: https://www.kernel.org/doc/html/latest/admin-guide/binderfs.html#mounting-binderfs

## rsb_hub

A comprehensive service manager for Linux that replaces Android's service_manager functionality.

### Usage
```bash
$ rsb_hub
```

### Features
**rsb_hub** provides a full-featured service management system with:

- **Service Registration**: Allows services to register themselves with unique names
- **Service Discovery**: Enables clients to find and connect to registered services
- **Lifecycle Management**: Monitors service health and handles cleanup
- **Priority Support**: Implements priority-based service access control
- **Notification System**: Provides callbacks for service availability changes
- **Debug Information**: Offers service introspection and debugging capabilities

### API Compatibility
**rsb_hub** implements the same interface as Android's service manager, ensuring compatibility with existing binder applications. It supports:

- `addService()`: Register a new service
- `getService()`: Retrieve a service by name
- `listServices()`: List all registered services
- `checkService()`: Check if a service exists
- `registerForNotifications()`: Register for service lifecycle notifications

### Implementation Details
Built on top of **rsbinder**'s service management APIs, **rsb_hub** provides:
- Thread-safe service registration and lookup
- Automatic cleanup of dead services
- Support for service priorities and access control
- Integration with Linux security models

The hub acts as a central registry that bridges the gap between service providers and consumers, making Binder IPC on Linux as seamless as on Android.
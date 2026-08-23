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
$ sudo rsb_device <device_name> [--group <GROUP>] [--mode <MODE>]
```

### Example
```bash
# Root-only device (the default)
$ sudo rsb_device binder

# Grant a group access -- the /dev/kvm model
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"
$ sudo rsb_device binder --group binder --mode 0660
```

### What it does
**rsb_device** uses the kernel's binderfs feature to create new binder device files and requires root privileges. It performs the following operations:

1. **Directory Creation**: Creates `/dev/binderfs` directory if it doesn't exist
2. **Filesystem Mount**: Executes `mount -t binder binder /dev/binderfs` to mount binderfs
3. **Device Creation**: Uses kernel ioctl interface to create `/dev/binderfs/<device_name>`
4. **Ownership and permissions**: Applies `--group` (unchanged by default) and `--mode` (default `0600`)

### Why the device is root-only by default
Binder has no in-kernel access control of its own, so the device node is the
only thing deciding who may speak binder at all -- exactly like `/dev/kvm`
being `0660 root:kvm`. `rsb_device` therefore creates it root-only and makes
you widen it deliberately. `rsb_hub` layers per-service policy on top of this;
the two are independent gates.

### Output
After successful execution, the binder device will be accessible at `/dev/binderfs/<device_name>` and ready for IPC operations.

For detailed technical information, refer to the [Linux kernel binderfs documentation][kernel_binder_doc].

[kernel_binder_doc]: https://www.kernel.org/doc/html/latest/admin-guide/binderfs.html#mounting-binderfs

## rsb_hub

A comprehensive service manager for Linux that replaces Android's service_manager functionality.

### Usage
```bash
# With an access-control policy (a file, or a directory of *.toml files;
# defaults to /etc/rsbinder/hub.d)
$ rsb_hub --policy /etc/rsbinder/hub.d

# With no access control at all -- development and test only
$ rsb_hub --insecure-allow-all
```

`rsb_hub` denies every request its policy does not allow, and refuses to start
when it cannot load one. `SIGHUP` reloads the policy in place; a reload that
fails keeps the policy already in force. See the
[Service Manager chapter](https://hiking90.github.io/rsbinder/service-manager.html#access-control)
for the file format.

### Features
**rsb_hub** provides a full-featured service management system with:

- **Service Registration**: Allows services to register themselves with unique names
- **Service Discovery**: Enables clients to find and connect to registered services
- **Lifecycle Management**: Monitors service health and handles cleanup
- **Access Control**: Per-name `add` / `find` / `list` policy keyed on caller uid and group, default-deny, reloadable with `SIGHUP`
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
- Access control keyed on the credentials the kernel vouches for (uid), with
  groups resolved through NSS -- portable to any Linux, and to macOS for the
  RPC transport, without requiring SELinux

The hub acts as a central registry that bridges the gap between service providers and consumers, making Binder IPC on Linux as seamless as on Android.
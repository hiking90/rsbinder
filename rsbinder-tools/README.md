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

The service manager for Linux — the counterpart of Android's `servicemanager`.

### Usage
```bash
# With an access-control policy (a file, or a directory of *.toml files;
# defaults to /etc/rsbinder/hub.d)
$ rsb_hub --config /etc/rsbinder/hub.d

# With no access control at all -- development and test only
$ rsb_hub --insecure-allow-all
```

`rsb_hub` denies every request its policy does not allow, and refuses to start
when it cannot load one. `SIGHUP` reloads the policy in place; a reload that
fails keeps the policy already in force. See the
[Service Manager chapter](https://hiking90.github.io/rsbinder/service-manager.html#access-control)
for the file format.

`SIGTERM` (and `SIGINT`) stop it cleanly, exiting 0 rather than dying by
signal. Under systemd use `Type=notify`: `rsb_hub` reports `READY=1` only once
it holds handle 0, so units ordered `After=` it never race the registry.

```ini
[Unit]
Description=rsbinder service manager
After=dev-binderfs.mount

[Service]
Type=notify
ExecStart=/usr/bin/rsb_hub --config /etc/rsbinder/hub.d
ExecReload=/bin/kill -HUP $MAINPID
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

A binder device has exactly one service manager. Starting a second `rsb_hub`
on the same device exits 1 and says so; give it its own device
(`rsb_device other && rsb_hub --device other`) to run an independent one.

### Features

- **Service Registration / Discovery**: services register under a unique name; clients look them up by it
- **Lifecycle Management**: dead services are reaped, and death notifications delivered
- **Access Control**: per-name `add` / `find` / `list` policy keyed on caller uid and group, default-deny, reloadable with `SIGHUP`
- **Service Declarations**: `[[service]]` entries answer `isDeclared` / `getDeclaredInstances` / `getConnectionInfo`, the Linux stand-in for VINTF manifests
- **On-Demand Start**: a lookup that misses a declared service starts it, via systemd or a configured command
- **Notification System**: callbacks for service availability changes
- **Dump Priorities**: `listServices` filters by the AOSP `DUMP_FLAG_PRIORITY_*` flags — a listing filter, not an access-control mechanism

### API Compatibility
**rsb_hub** implements the same interface as Android's service manager, ensuring compatibility with existing binder applications. It supports:

- `addService()`: Register a new service
- `getService()`: Retrieve a service by name
- `listServices()`: List all registered services
- `checkService()`: Check if a service exists
- `registerForNotifications()`: Register for service lifecycle notifications

### Implementation Details

Access control is keyed on the credential the kernel vouches for — the caller
uid the binder driver fills in itself — with groups resolved through NSS. That
is what makes the policy portable to any Linux without requiring SELinux.
Deliberately unused: **pid**, because pid reuse makes every pid-derived
attribute unsound as an authorization key.

## rsb_service

Ask the running service manager what it knows -- the Linux counterpart of
Android's `service` and `dumpsys -l`.

### Usage
```bash
$ rsb_service list                                  # what is registered
$ rsb_service info                                  # ... and which pid owns each
$ rsb_service check com.example.IFoo/default        # is it up yet?
$ rsb_service declared com.example.IFoo/default     # is it even configured?
$ rsb_service instances com.example.IFoo            # every declared instance
$ rsb_service connection com.example.IFoo/default   # declared ip:port, if any
$ rsb_service dump manager                          # rsb_hub's own registry
$ rsb_service dump manager com.example              # ... narrowed by substring
```

Exit status is the answer: `0` yes, `1` no (not registered, not declared), `2`
the question could not be answered -- no service manager, or its policy denied
it. So `rsb_service check foo || start-foo` reads the way it looks.

`rsb_service` is an ordinary binder client, so `rsb_hub`'s policy applies to it
like anything else: `list`, `info` and `dump` need `list`, and every name they
report is filtered by `find`. A denial is reported as a denial rather than as
an empty result -- except for `check`, where the hub answers a denied lookup
exactly as it answers an unregistered name, by design, so that a denied caller
cannot use it to enumerate which names exist.

`dump <name>` works on any service, not just the hub: it sends
`DUMP_TRANSACTION` and prints whatever the service writes, which for an
rsbinder service is its `Interface::dump` implementation.

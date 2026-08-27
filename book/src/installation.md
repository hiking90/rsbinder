# Installation

## Prerequisites

### Rust Version Requirements
**rsbinder** requires Rust 1.85 or later. Ensure you have the latest stable Rust toolchain:
```bash
$ rustup update stable
$ rustc --version  # Should be 1.85+
```

## Enable binder for Linux
Please refer to [Enable binder for Linux](./enable-binder-for-linux.md) for detailed instructions on setting it up.

## Create binder device file for Linux
After the binder configuration of the Linux kernel is complete, a binder device file must be created.

### Method 1: Install from crates.io
Install **rsbinder-tools** and run to create a binder device:

```bash
$ cargo install rsbinder-tools
# Create the `binder` group and put yourself in it (log out and back in,
# or use `newgrp binder`, for the membership to take effect)
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"

# Create the device, owned by that group. The node's mode is the only gate
# on who may speak binder at all, so it defaults to 0600 (root only).
$ sudo rsb_device binder --group binder --mode 0660
```

### Method 2: Build from source
If you prefer to build from source:

```bash
$ git clone https://github.com/hiking90/rsbinder.git
$ cd rsbinder
$ cargo build --release
# Create the `binder` group and put yourself in it (log out and back in,
# or use `newgrp binder`, for the membership to take effect)
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"

# Create the device, owned by that group. The node's mode is the only gate
# on who may speak binder at all, so it defaults to 0600 (root only).
$ sudo target/release/rsb_device binder --group binder --mode 0660
```

The `rsb_device` tool will:
- Create `/dev/binderfs` directory if it doesn't exist
- Mount binderfs filesystem
- Create the specified binder device
- Set the device node's owning group (`--group`) and mode (`--mode`, default
  `0600`)

The mode is not a formality: binder has no in-kernel access control of its
own, so the device node is the only thing deciding who can speak binder at
all — the same model as `/dev/kvm` being `0660 root:kvm`. It defaults to
root-only; grant access deliberately with `--group`.

## Run a service manager for Linux
If **rsbinder-tools** is already installed, the **rsb_hub** executable is also installed.

`rsb_hub` denies every request that its policy does not allow, and refuses to
start when it cannot load one, so a first run needs one of two things. For a
quick local try-out, run with no access control at all:

```bash
$ rsb_hub --insecure-allow-all
```

For anything else, write a policy — see
[Access control](./service-manager.md#access-control) — and point `rsb_hub` at
it (`/etc/rsbinder/hub.d` is the default):

```bash
$ rsb_hub --config /etc/rsbinder/hub.d
```

Alternatively, if building from source:

```bash
$ cargo run --bin rsb_hub -- --insecure-allow-all
```

The service manager (rsb_hub) provides:
- Service registration and discovery
- Service lifecycle management
- Priority-based service access

### Using a custom binder device path

By default, `rsb_hub` uses `/dev/binderfs/binder`. If you created a binder device with a different name, use the `--device` (`-d`) option:

```bash
$ rsb_hub --device custom_binder
# or
$ rsb_hub -d custom_binder
```

In your Rust code, use `ProcessState::init()` instead of `ProcessState::init_default()` to specify the matching path:

```rust
// Use a custom binder device path
ProcessState::init(
    "/dev/binderfs/custom_binder",
    rsbinder::DEFAULT_MAX_BINDER_THREADS,
)?;
```

> **Important**: The service manager, service, and client must all use the **same binder device path**.

## Dependencies for **rsbinder** projects
Add the following configuration to your Cargo.toml file:

```toml
[dependencies]
rsbinder = "0.10"
async-trait = "0.1"
env_logger = "0.11"  # Optional: for logging

[build-dependencies]
rsbinder-aidl = "0.10"
```

### Feature Flags
**rsbinder** supports various feature flags for different use cases:

```toml
[dependencies]
rsbinder = "0.10"  # Default: includes tokio async runtime
# or
rsbinder = { version = "0.10", features = ["async"] }  # Async trait support without tokio — use your own async runtime
# or
rsbinder = { version = "0.10", default-features = false }  # Synchronous only (no async support)
# or — opt in to the binder-over-socket stack (see RPC Transport chapter)
rsbinder = { version = "0.10", features = ["rpc"] }
```

Available features:
- `tokio` (default): Full tokio async runtime support (includes `async` feature)
- `async`: Async trait support without tokio runtime — use this when integrating with a different async runtime
- `rpc`: Enable the RPC transport (binder-over-socket). Add `rpc-vsock`, `rpc-tls`, or `rpc-tcp-debug` to bundle the matching backend. See [RPC Transport](./rpc-transport.md).
- `android_*`: Android version compatibility flags (see [Android Development](./android.md))

### Crate Purposes:
- **rsbinder**: Core library providing Binder IPC functionality, including kernel communication, data serialization/deserialization, thread pool management, and service lifecycle.
- **async-trait**: Required for async interface implementations generated by **rsbinder-aidl**.
- **rsbinder-aidl**: AIDL-to-Rust code generator, used in build.rs for compile-time code generation.
- **env_logger**: Optional but recommended for debugging and development logging.

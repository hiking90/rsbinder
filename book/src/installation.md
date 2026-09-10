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
Once the kernel has binder support, create the device node.

First, create the group that will own the device and put yourself in it (log
out and back in, or run `newgrp binder`, for the membership to take effect):

```bash
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"
```

Then install **rsbinder-tools** and create the device:

```bash
$ cargo install rsbinder-tools
$ sudo rsb_device binder --group binder --mode 0660
```

Or, building from source:

```bash
$ git clone https://github.com/hiking90/rsbinder.git
$ cd rsbinder
$ cargo build --release
$ sudo target/release/rsb_device binder --group binder --mode 0660
```

`rsb_device` creates `/dev/binderfs` if it does not exist, mounts binderfs,
creates the named device, and applies `--group` and `--mode`.

The mode is not a formality: binder has no in-kernel access control of its
own, so the device node is the only thing deciding who can speak binder at
all — the same model as `/dev/kvm` being `0660 root:kvm`. It defaults to
`0600`, root only; grant access deliberately with `--group`.

## Run a service manager for Linux
Installing **rsbinder-tools** also installs **rsb_hub**.

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

`rsb_hub` is the registry every service registers with and every client looks
up through. It also enforces per-name access control, and can declare and
start services on demand — see [Service Manager](./service-manager.md).

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
rsbinder = "0.11"
async-trait = "0.1"
env_logger = "0.11"  # Optional: for logging

[build-dependencies]
rsbinder-aidl = "0.11"
```

### Feature flags

```toml
rsbinder = "0.11"                                          # default: tokio async
rsbinder = { version = "0.11", default-features = false }   # synchronous only
rsbinder = { version = "0.11", features = ["rpc", "macros"] }
```

- `tokio` (default) — full tokio async runtime support (implies `async`).
- `async` — async-trait support without tokio, for a different runtime.
- `rpc` — the RPC transport (binder-over-socket). Add `rpc-vsock`, `rpc-tls`,
  or `rpc-tcp-debug` for the matching backend. See
  [RPC Transport](./rpc-transport.md).
- `macros` — `#[rsbinder::interface]` and the `Parcelable` / `BinderEnum`
  derives: an interface as a Rust trait, with no `.aidl` file and no
  `build.rs`. See [Interface Macros](./interface-macros.md).
- `android_*` — Android version compatibility (see
  [Android Development](./android.md)).

### What each crate is for

- **rsbinder** — the core library: kernel communication, parcel
  serialization, thread pool, service lifecycle.
- **rsbinder-aidl** — the AIDL-to-Rust generator, used from `build.rs`.
- **async-trait** — required by the async interfaces `rsbinder-aidl`
  generates.
- **env_logger** — optional, but the fastest way to see what rsbinder is
  doing.

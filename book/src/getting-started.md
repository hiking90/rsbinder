# Getting Started

## Pick a stack

**rsbinder** ships two, and which one you can use depends on the platform:

- **Kernel binder** — Linux 5.0+ with binderfs enabled, or Android. Talks to
  the kernel driver through `/dev/binderfs/binder` (Linux) or `/dev/binder`
  (Android). This is what most of this guide covers.
- **RPC transport** (binder-over-socket, opt-in via the `rpc` feature) — pure
  user-space, no kernel driver, no root. Runs on Linux, Android, and
  **macOS**, over Unix-domain sockets, vsock, or TLS. See
  [RPC Transport](./rpc-transport.md).

macOS has no binder driver, so only the RPC stack works there — enough to
develop and test RPC services on a workstation without a Linux VM. Windows is
not supported on either stack.

## Before you start

- [ ] Rust 1.85 or later
- [ ] For kernel binder: a kernel with binder support, or an Android
      device/emulator
- [ ] For kernel binder on Linux: a device node created with `rsb_device`, and
      `rsb_hub` running as the service manager

[Installation](./installation.md) walks through all of these.

## The workflow

1. Define the interface — an `.aidl` file, or a Rust trait with
   [`#[rsbinder::interface]`](./interface-macros.md).
2. Generate the Rust from it (`rsbinder-aidl` in a `build.rs`; the macro needs
   no build step).
3. Implement the service.
4. Register it and connect to it — `rsbinder::serve(uri)` and
   `rsbinder::connect(uri)`.

[Hello, World!](./hello-world.md) does all four end to end, and is the fastest
way to see where each piece fits.

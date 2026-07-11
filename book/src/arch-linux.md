# Enable Binder IPC on Arch Linux

Arch Linux provides an easy way to enable Binder IPC support through the linux-zen kernel, which already includes all necessary Binder components.

## Install linux-zen Kernel

The linux-zen kernel is the recommended and simplest method to get Binder IPC support on Arch Linux:

```bash
# Update system packages
$ sudo pacman -Syu

# Install linux-zen kernel and headers
$ sudo pacman -S linux-zen linux-zen-headers

# Update bootloader configuration
$ sudo grub-mkconfig -o /boot/grub/grub.cfg

# Reboot to use the new kernel
$ sudo reboot
```

After reboot, select the zen kernel from the GRUB menu or set it as default.

## Verification

After installing and booting into the zen kernel, verify Binder support is available:

```bash
# Check current kernel
$ uname -r
# Should show something like "6.x.x-zen1-1-zen"
```

## Install rsbinder-tools

Install the rsbinder development tools:

```bash
# Install Rust (if not already installed)
$ sudo pacman -S rustup
$ rustup default stable

# Install rsbinder-tools from crates.io
$ cargo install rsbinder-tools
```

## Create and Test Binder Device

Create a binder device and test the setup:

```bash
# Create binder device (binderfs devices are not persistent —
# re-run this after each reboot)
$ sudo rsb_device binder

# Verify device creation
$ ls -la /dev/binderfs/binder

# Test with a simple example
$ git clone https://github.com/hiking90/rsbinder.git
$ cd rsbinder

# Start service manager in one terminal
$ rsb_hub

# In another terminal, run the example
$ cargo run --bin hello_service &
$ cargo run --bin hello_client
```

## Persistent Configuration

The linux-zen kernel builds binder directly into the kernel
(`CONFIG_ANDROID_BINDER_IPC=y`), so there is no module to load on boot —
no `modules-load.d` or `modprobe` configuration is needed. The only
non-persistent piece is the binderfs device itself: re-run
`sudo rsb_device binder` after each reboot (see above).

> **Note**: `binder_linux` is the out-of-tree Anbox DKMS module, not
> part of the zen kernel. `modules-load.d` / `modprobe binder_linux`
> instructions found elsewhere apply only to that third-party module
> setup on kernels without built-in binder support.

## Troubleshooting

If you encounter issues:

```bash
# Check kernel messages for binder-related errors
$ dmesg | grep -i binder

# Verify zen kernel is running
$ uname -r | grep zen

# Verify binder is built into the kernel config
$ zgrep -E "(ANDROID|BINDER)" /proc/config.gz
```

## References

- [Arch Linux Kernel Documentation](https://wiki.archlinux.org/title/kernel)
- [Linux-zen Kernel](https://wiki.archlinux.org/title/Kernel#linux-zen)

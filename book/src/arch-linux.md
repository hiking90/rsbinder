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
# Create binder device
$ sudo rsb_device binder

# Verify device creation
$ ls -la /dev/binderfs/binder

# Test with a simple example
$ git clone https://github.com/hiking90/rsbinder.git
$ cd rsbinder/example-hello

# Start service manager in one terminal
$ rsb_hub

# In another terminal, run the example
$ cargo run --bin hello_service &
$ cargo run --bin hello_client
```

## Persistent Configuration

To automatically load binder modules on boot:

```bash
# Create module loading configuration
$ echo "binder_linux" | sudo tee /etc/modules-load.d/binder.conf

# Set module parameters
$ echo "options binder_linux devices=binder,hwbinder,vndbinder" | sudo tee /etc/modprobe.d/binder.conf
```

## Troubleshooting

If you encounter issues:

```bash
# Check kernel messages for binder-related errors
$ dmesg | grep -i binder

# Verify zen kernel is running
$ uname -r | grep zen

# Check if modules loaded successfully
$ sudo modprobe -v binder_linux
```

## References

- [Arch Linux Kernel Documentation](https://wiki.archlinux.org/title/kernel)
- [Linux-zen Kernel](https://wiki.archlinux.org/title/Kernel#linux-zen)

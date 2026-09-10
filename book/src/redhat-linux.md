# Enable Binder IPC on RedHat Linux (RHEL/CentOS/Fedora)

> **Note**: This guide is community-contributed and may require adjustments for your specific system configuration. Please test in a safe environment first.

RedHat-based distributions (RHEL, CentOS, Fedora) do not include Binder IPC support by default, so enabling it means building a kernel.

## Fedora

Fedora provides kernel source packages that can be modified to include binder support:

```bash
# Install development tools
$ sudo dnf groupinstall "Development Tools"
$ sudo dnf install fedora-packager fedpkg
$ sudo dnf install kernel-devel kernel-headers

# Install kernel build dependencies
$ sudo dnf builddep kernel

# Get kernel source
$ fedpkg clone -a kernel
$ cd kernel
$ fedpkg switch-branch f$(rpm -E %fedora)
$ fedpkg prep

# Modify kernel config to enable binder
$ cd ~/rpmbuild/BUILD/kernel-*/linux-*
$ make menuconfig

# Enable the following options:
# General setup -> Android support (CONFIG_ANDROID=y)
# CONFIG_ANDROID_BINDER_IPC=y
# CONFIG_ANDROID_BINDERFS=y

# Build the kernel
$ make -j$(nproc)
$ sudo make modules_install
$ sudo make install

# Update bootloader
$ sudo grub2-mkconfig -o /boot/grub2/grub.cfg
$ sudo reboot
```

## RHEL/CentOS

### Build a Custom Kernel

For enterprise distributions, building a custom kernel is often the most reliable approach:

```bash
# Install EPEL repository (CentOS/RHEL 8+)
$ sudo dnf install epel-release

# Install development tools
$ sudo dnf groupinstall "Development Tools"
$ sudo dnf install rpm-build rpm-devel libtool

# Install kernel build dependencies
$ sudo dnf install kernel-devel kernel-headers
$ sudo dnf install elfutils-libelf-devel openssl-devel

# Download kernel source matching your running kernel.
# Pick the v<major>.x directory that matches your kernel: today this
# is typically v6.x on Fedora/Stream; older RHEL 8/9 hosts may still
# be on v5.x or v4.x.
$ KERNEL_VERSION=$(uname -r | sed 's/\.el.*$//')
$ KERNEL_MAJOR=$(echo $KERNEL_VERSION | cut -d. -f1)
$ wget https://cdn.kernel.org/pub/linux/kernel/v${KERNEL_MAJOR}.x/linux-${KERNEL_VERSION}.tar.xz
$ tar -xf linux-${KERNEL_VERSION}.tar.xz
$ cd linux-${KERNEL_VERSION}

# Use current kernel config as base
$ zcat /proc/config.gz > .config
# or
$ cp /boot/config-$(uname -r) .config

# Modify config to enable binder
$ make menuconfig
# Enable CONFIG_ANDROID=y, CONFIG_ANDROID_BINDER_IPC=y, CONFIG_ANDROID_BINDERFS=y

# Build and install
$ make -j$(nproc)
$ sudo make modules_install
$ sudo make install

# Update GRUB
$ sudo grub2-mkconfig -o /boot/grub2/grub.cfg
$ sudo reboot
```

## CentOS Stream

CentOS Stream may have more recent kernels that could include binder support:

```bash
# Check current kernel version
$ uname -r

# Update to latest kernel
$ sudo dnf update kernel

# Check if binder is already available
$ grep -E "(ANDROID|BINDER)" /boot/config-$(uname -r)
```

## After Kernel Build

With the kernel built as described above, binder is compiled directly
into the kernel (`CONFIG_ANDROID_BINDER_IPC=y` is a built-in option,
not a module), so nothing needs to be loaded with `modprobe` or
configured in `modules-load.d`:

```bash
# Verify binder is built into the running kernel
$ grep -E "(ANDROID|BINDER)" /boot/config-$(uname -r)
```

> **Note**: `modprobe binder_linux` / `modules-load.d` instructions
> found elsewhere refer to `binder_linux`, the out-of-tree Anbox DKMS
> module for kernels without built-in binder support. They do not apply
> to a kernel built with the options above.

## SELinux Considerations

RedHat systems run SELinux, which can deny access to the binder device even
once the node exists and its mode is right. A denial looks like a failed
`open` rather than an rsbinder error, so check the audit log before suspecting
anything else:

```bash
$ sestatus
$ sudo ausearch -m avc -ts recent | grep -i binder
```

The fix is a policy module allowing your domain to use the device. Putting the
whole system in permissive mode (`setenforce 0`) will also make the denial go
away, but it disables SELinux for everything else on the machine — reach for it
only to confirm a diagnosis, never as the resting state.

## Verification

Test that binder is working:

```bash
# Check if binderfs is available
$ grep binderfs /proc/filesystems

# Install rsbinder-tools and create binder device
$ cargo install rsbinder-tools
# Create the `binder` group and put yourself in it (log out and back in,
# or use `newgrp binder`, for the membership to take effect)
$ sudo groupadd -f binder
$ sudo usermod -aG binder "$USER"

# Create the device, owned by that group. The node's mode is the only gate
# on who may speak binder at all, so it defaults to 0600 (root only).
$ sudo rsb_device binder --group binder --mode 0660

# Verify device creation
$ ls -la /dev/binderfs/binder
```

## Troubleshooting

### Common Issues:

1. **Module compilation fails**: Ensure all kernel-devel packages match your running kernel
2. **SELinux denials**: Check `audit.log` for SELinux denials and create appropriate policies
3. **Kernel version mismatch**: Ensure kernel source matches your running kernel version

### Debugging:

```bash
# Check kernel messages
$ dmesg | grep -i binder

# Check system journal
$ journalctl -f | grep -i binder

# Verify kernel config
$ grep -E "(ANDROID|BINDER)" /boot/config-$(uname -r)
```

## References

- [RedHat Kernel Documentation](https://access.redhat.com/documentation/en-us/red_hat_enterprise_linux/8/html/managing_monitoring_and_updating_the_kernel/)
- [Fedora Kernel Building](https://fedoraproject.org/wiki/Building_a_custom_kernel)
- [CentOS Custom Kernels](https://wiki.centos.org/HowTos/Custom_Kernel)

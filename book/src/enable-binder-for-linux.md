# Enable binder for Linux
Most Linux distributions do not have Binder IPC enabled by default, so additional steps are required to use it.

> **Note**: The binder driver was mainlined in Linux kernel 4.17, but **binderfs** — which rsbinder's `rsb_device` tool uses to create binder devices — landed in kernel 5.0. In practice you need kernel 5.0 or later.

If you are able to build the Linux kernel yourself, you can enable Binder IPC by adding the following kernel configuration options:
```
CONFIG_ANDROID=y
CONFIG_ANDROID_BINDER_IPC=y
CONFIG_ANDROID_BINDERFS=y
```

## Distribution-Specific Guides

Select your Linux distribution for detailed setup instructions:

- **[Arch Linux](./arch-linux.md)** - Uses the `linux-zen` kernel (simplest method)
- **[Ubuntu Linux](./ubuntu-linux.md)** - Custom kernel build or module installation
- **[RedHat Linux](./redhat-linux.md)** - RHEL, CentOS, and Fedora instructions

## Other documents related to Binder IPC
- [Linux kernel's Binder Document](https://www.kernel.org/doc/html/latest/admin-guide/binderfs.html)
- [Anbox Document](https://wiki.archlinux.org/title/Anbox#Mounting_binderfs)

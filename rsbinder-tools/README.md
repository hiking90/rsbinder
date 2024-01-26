# rsbinder-tools
This crate provides the following CLI commands.

## rsb_device
rsb_device uses the binderfs feature of kernel to create a new binder device file.
rsb_device requires root privileges and performs the following tasks:

* Create a /dev/binderfs folder.
* Execute the command 'mount -t binder binder /dev/binderfs'.
* Use the ioctl feature provided by the kernel to create "/dev/binderfs/device_name".
* Change the permissions of "/dev/binderfs/device_name" so that all users can read and write.

For detailed technical information, refer to the [Linux kernel documentation][kernel_binder_doc].

[kernel_binder_doc]: https://www.kernel.org/doc/html/latest/admin-guide/binderfs.html#mounting-binderfs

## rsb_hub
**rsb_hub** is the servicemanager provided by **crate rsbinder**.
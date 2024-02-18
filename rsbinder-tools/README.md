# rsbinder-tools
This crate provides CLI tools for Linux.
While Android has several tools prepared for binder IPC, additional tools are required to use Binder IPC in Linux.

## rsb_device
This is a utility that helps with the initialization of the Linux binder environment.

The following command creates the /dev/binderfs/binder device file, and the user can specify the file name.
```
$ sudo target/debug/rsb_device binder
```

**rsb_device** uses the binderfs feature of kernel to create a new binder device file.
**rsb_device** requires root privileges and performs the following tasks:

* Create a /dev/binderfs folder.
* Execute the command 'mount -t binder binder /dev/binderfs'.
* Use the ioctl feature provided by the kernel to create "/dev/binderfs/device_name".
* Change the permissions of "/dev/binderfs/device_name" so that all users can read and write.

For detailed technical information, refer to the [Linux kernel documentation][kernel_binder_doc].

[kernel_binder_doc]: https://www.kernel.org/doc/html/latest/admin-guide/binderfs.html#mounting-binderfs

## rsb_hub
**rsb_hub** is a tool designed to replace Android's service_manager.

It is implemented using the Service APIs provided by the crate **rsbinder_hub**. The Client APIs offered by crate **rsbinder_hub** facilitate communication with **rsb_hub**, allowing for the registration of new services and the discovery and management of existing services.
use std::path::Path;
use std::fs::File;
use std::os::unix::io::{AsRawFd};
use log;
use crate::sys::binder;
use std::ffi::CString;

pub fn add_device(driver: &Path, name: &str) -> std::io::Result<(u32, u32)> {
    let fd = File::options()
        .read(true)
        .open(driver)
        .map_err(|e| {
            log::error!("Opening '{}' failed: {}\n", driver.to_string_lossy(), e.to_string());
            e
        })?;

    let mut device = binder::binderfs_device {
        name: [0; 256],
        major: 0,
        minor: 0,
    };

    for (a, c) in device.name.iter_mut().zip(CString::new(name)?.as_bytes_with_nul()) {
        *a = *c as i8;
    }

    unsafe {
        binder::binder_ctl_add(fd.as_raw_fd(), &mut device)
            .map_err(|e| {
                log::error!("Binder ioctl to add binder failed: {}", e.to_string());
                e
            })?;
    }

    drop(fd);

    Ok((device.major, device.minor))
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::fs::File;
use log;
use crate::sys::binder;
use std::ffi::CString;

/// Add a new binder device to the binderfs.
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
        *a = *c as std::os::raw::c_char;
    }

    #[cfg(not(test))]
    binder::binder_ctl_add(fd, &mut device)
        .map_err(|e| {
            log::error!("Binder ioctl to add binder failed: {}", e.to_string());
            e
        })?;

    #[cfg(test)]
    tests::binder_ctl_add(fd, &mut device)
        .map_err(|e| {
            log::error!("Binder ioctl to add binder failed: {}", e.to_string());
            e
        })?;

    Ok((device.major, device.minor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    pub(crate) fn binder_ctl_add<Fd: AsFd>(_fd: Fd, device: &mut binder::binderfs_device) -> std::result::Result<(), rustix::io::Errno> {
        device.major = 511;
        device.minor = 0;
        Ok(())
    }

    #[test]
    fn test_add_device() {
        let driver = Path::new("/dev/binder");
        let name = "rsbinder";
        let (major, minor) = add_device(driver, name).unwrap();
        assert_eq!(major, 511);
        assert_eq!(minor, 0);
    }

    #[test]
    fn test_add_device_error() {
        let driver = Path::new("/dev/binder_error");
        let name = "rsbinder";
        let result = add_device(driver, name);
        assert!(result.is_err());
    }
}

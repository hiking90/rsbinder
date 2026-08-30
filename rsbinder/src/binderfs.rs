// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! BinderFS filesystem utilities.
//!
//! This module provides functions for managing binder devices in the binderfs
//! filesystem, including adding new binder devices dynamically.

use crate::sys::binder;
use log;
use std::ffi::CString;
use std::fs::File;
use std::path::Path;

/// Add a new binder device to the binderfs.
///
/// Creates a new binder device node in the binderfs with the specified name.
/// Returns the major and minor device numbers on success.
pub fn add_device(driver: &Path, name: &str) -> std::io::Result<(u32, u32)> {
    let fd = File::options().read(true).open(driver).inspect_err(|e| {
        log::error!("Opening '{}' failed: {}\n", driver.to_string_lossy(), e);
    })?;

    let mut device = binder::binderfs_device {
        name: [0; 256],
        major: 0,
        minor: 0,
    };

    let cname = CString::new(name)?;
    let name_bytes = cname.as_bytes_with_nul();
    // `zip` would otherwise silently truncate a name that does not fit,
    // leaving the 256-byte field without a NUL terminator and handing the
    // kernel a mis-named device. Fail loudly instead.
    if name_bytes.len() > device.name.len() {
        log::error!(
            "Binder device name too long: {} bytes (max {})",
            name_bytes.len(),
            device.name.len()
        );
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "binder device name too long",
        ));
    }
    for (a, c) in device.name.iter_mut().zip(name_bytes) {
        *a = *c as std::os::raw::c_char;
    }

    #[cfg(not(test))]
    binder::binder_ctl_add(fd, &mut device).inspect_err(|e| {
        log::error!("Binder ioctl to add binder failed: {e}");
    })?;

    #[cfg(test)]
    tests::binder_ctl_add(fd, &mut device).inspect_err(|e| {
        log::error!("Binder ioctl to add binder failed: {e}");
    })?;

    Ok((device.major, device.minor))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    /// Stand-in for the `BINDER_CTL_ADD` ioctl that checks what `add_device`
    /// put in the struct — a NUL-terminated copy of the name — and derives
    /// the returned numbers from it, so a broken copy loop fails the test
    /// instead of being masked by hard-coded values.
    pub(crate) fn binder_ctl_add<Fd: AsFd>(
        _fd: Fd,
        device: &mut binder::binderfs_device,
    ) -> std::result::Result<(), rustix::io::Errno> {
        let bytes: Vec<u8> = device.name.to_vec();
        let nul = bytes
            .iter()
            .position(|&b| b == 0)
            .ok_or(rustix::io::Errno::INVAL)?;
        let name = std::str::from_utf8(&bytes[..nul]).map_err(|_| rustix::io::Errno::INVAL)?;
        if name.is_empty() {
            return Err(rustix::io::Errno::INVAL);
        }
        device.major = 511;
        device.minor = name.len() as u32;
        Ok(())
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "android")),
        ignore = "requires /dev/binder"
    )]
    fn test_add_device() {
        let driver = Path::new("/dev/binder");
        let name = "rsbinder";
        let (major, minor) = add_device(driver, name).unwrap();
        assert_eq!(major, 511);
        assert_eq!(
            minor,
            name.len() as u32,
            "mock derives minor from the copied name"
        );
    }

    #[test]
    fn test_add_device_rejects_overlong_name() {
        let name = "x".repeat(256);
        let err = add_device(Path::new("/dev/null"), &name).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
    }

    #[test]
    fn test_add_device_error() {
        let driver = Path::new("/dev/binder_error");
        let name = "rsbinder";
        let result = add_device(driver, name);
        assert!(result.is_err());
    }
}

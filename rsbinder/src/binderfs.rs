// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::fs::File;
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
        *a = *c as std::os::raw::c_char;
    }

    binder::binder_ctl_add(fd, &mut device)
        .map_err(|e| {
            log::error!("Binder ioctl to add binder failed: {}", e.to_string());
            e
        })?;

    Ok((device.major, device.minor))
}

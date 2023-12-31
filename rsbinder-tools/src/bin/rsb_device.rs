// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::os::unix::fs::PermissionsExt;

use env_logger;
use std::process::Command;
use rsbinder::*;

fn is_mounted() -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap();
    mounts.contains("binder")
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        eprintln!("Usage: rsb_device <device_name>");
        return Ok(())
    }

    env_logger::init();

    let binderfs_path = Path::new(DEFAULT_BINDERFS_PATH);
    let control_path = Path::new(DEFAULT_BINDER_CONTROL_PATH);

    // Create binder control path if it doesn't exist.
    if !binderfs_path.exists() {
        println!("{} doesn't exist. Creating...", binderfs_path.display());
        std::fs::create_dir_all(binderfs_path)?;
    }

    // Check if binder control path is a directory.
    if !binderfs_path.is_dir() {
        eprintln!("{} is not a directory", DEFAULT_BINDERFS_PATH);
        return Ok(())
    }

    // Mount binderfs if it is not mounted.
    if !is_mounted() {
        println!("Mounting binderfs on {}", DEFAULT_BINDERFS_PATH);
        let status = Command::new("mount")
            .arg("-t")
            .arg("binder")
            .arg("binder")
            .arg(DEFAULT_BINDERFS_PATH)
            .status()?;

        if status.success() {
            println!("binder mounted successfully on /dev/binderfs");
        } else {
            eprintln!("Failed to mount binder");
            return Ok(())
        }
    }

    // Add binder device.
    let device_name = args[1].as_str();
    binderfs::add_device(control_path, device_name)
        .map(|(major, minor)| {
            println!("Allocated new binder device with major {major}, minor {minor}, and name [{}]", device_name);

            let device_path = format!("{}/{}", DEFAULT_BINDERFS_PATH, device_name);
            let mut perms = std::fs::metadata(device_path.as_str()).expect("IO error").permissions();
            perms.set_mode(0o666);
            std::fs::set_permissions(device_path.as_str(), perms).expect("IO error");

            println!("The permission of device path({}) has been changed to 0666", device_path);
        })?;

    Ok(())
}
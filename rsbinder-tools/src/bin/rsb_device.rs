// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
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

    let control_path = Path::new(DEFAULT_BINDER_CONTROL_PATH);

    // Create binder control path if it doesn't exist.
    if !control_path.exists() {
        std::fs::create_dir_all(control_path)?;
    }

    // Check if binder control path is a directory.
    if !control_path.is_dir() {
        eprintln!("{} is not a directory", DEFAULT_BINDER_CONTROL_PATH);
        return Ok(())
    }

    // Mount binderfs if it is not mounted.
    if !is_mounted() {
        let status = Command::new("mount")
            .arg("-t")
            .arg("binder")
            .arg("binder")
            .arg("/dev/binderfs")
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
            println!("Allocated new binder device with major {}, minor {}, and name {}", major, minor, device_name);
        })?;

    Ok(())
}
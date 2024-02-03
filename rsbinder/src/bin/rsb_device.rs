// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;
use std::os::unix::fs::PermissionsExt;

use std::process::Command;
use rsbinder::*;
use anstyle::*;

fn is_mounted() -> bool {
    let mounts = std::fs::read_to_string("/proc/mounts").unwrap();
    mounts.contains("binder")
}

fn log_ok(msg: &str) {
    let style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    println!("[{}OK{}] {}", style.render(), style.render_reset(), msg);
}

fn log_err(msg: &str) {
    log::error!("{}", msg);
    std::process::exit(1);
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 2 {
        println!("rsb_device - Create a binder device on binderfs");
        println!("Usage: rsb_device <device_name>");
        return Ok(())
    }

    env_logger::init();

    let binderfs_path = Path::new(DEFAULT_BINDERFS_PATH);
    let control_path = Path::new(DEFAULT_BINDER_CONTROL_PATH);

    // Create binder control path if it doesn't exist.
    if !binderfs_path.exists() {
        std::fs::create_dir_all(binderfs_path)
            .map(|_| log_ok(&format!("{} created successfully", binderfs_path.display())))
            .map_err(|err| log_err(&format!("Failed to create {}\n{}", binderfs_path.display(), err))).ok();
    } else {
        log_ok(&format!("{} already exists", binderfs_path.display()));
    }

    // Check if binder control path is a directory.
    if !binderfs_path.is_dir() {
        log_err(&format!("{} is not a directory", DEFAULT_BINDERFS_PATH));
    }

    // Mount binderfs if it is not mounted.
    if !is_mounted() {
        // println!("Mounting binderfs on {}", DEFAULT_BINDERFS_PATH);
        Command::new("mount")
            .arg("-t")
            .arg("binder")
            .arg("binder")
            .arg(DEFAULT_BINDERFS_PATH)
            .status()
            .map(|_| log_ok(&format!("binder mounted successfully on {}", DEFAULT_BINDERFS_PATH)))
            .map_err(|err| log_err(&format!("Failed to mount binderfs\n{}", err))).ok();
    } else {
        log_ok(&format!("binderfs is already mounted on {}", DEFAULT_BINDERFS_PATH));
    }

    // Add binder device.
    let device_name = args[1].as_str();
    binderfs::add_device(control_path, device_name)
        .map(|(major, minor)| {
            log_ok(&format!("Allocated new binder device with major {major}, minor {minor}, and name [{}]", device_name));

            let device_path = format!("{}/{}", DEFAULT_BINDERFS_PATH, device_name);
            let mut perms = std::fs::metadata(device_path.as_str()).expect("IO error").permissions();
            perms.set_mode(0o666);
            std::fs::set_permissions(device_path.as_str(), perms)
                .map(|_| log_ok(&format!("The permission of device path({}) has been changed to 0666", device_path)))
                .map_err(|err| log_err(&format!("Failed to change the permission of device path({}) to 0666\n{}", device_path, err))).ok();
        })?;

    Ok(())
}
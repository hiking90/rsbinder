// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use std::process::Command;

use anstyle::*;
use rsbinder::*;

/// Returns `true` if `binderfs_path` is a current mount point in `/proc/mounts`.
///
/// Matches the mount target field exactly: substring matching against
/// `"binder"` aliases other binderfs mounts (`hwbinder` / `vndbinder`).
fn is_mounted(binderfs_path: &Path) -> std::io::Result<bool> {
    let mounts = std::fs::read_to_string("/proc/mounts")?;
    Ok(mounts_text_contains_target(&mounts, binderfs_path))
}

/// Pure parser split off from `is_mounted` for unit-testability without
/// `/proc` access.
fn mounts_text_contains_target(mounts: &str, target: &Path) -> bool {
    mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|t| Path::new(t) == target)
    })
}

fn log_ok(msg: &str) {
    let style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    println!("[{}OK{}] {}", style.render(), style.render_reset(), msg);
}

fn log_err(msg: &str) -> ! {
    log::error!("{msg}");
    std::process::exit(1);
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    let app = clap::Command::new("rsb_device")
        .version(env!("CARGO_PKG_VERSION"))
        .author(env!("CARGO_PKG_AUTHORS"))
        .about("Creates a binder device on binderfs")
        .arg(clap::Arg::new("device_name")
             .help("Name of the binder device to create on binderfs, accessible via /dev/binderfs/<device_name>")
             .required(true)
             .index(1))
        .after_help("Examples:\n    \
            Create a new binder device named 'mybinder':\n    \
            $ ./rsb_device mybinder\n    \
            This command will create a device accessible at /dev/binderfs/mybinder.\n\n\
            Create a new binder device named 'test_device':\n    \
            $ ./rsb_device test_device\n    \
            This command will create a device accessible at /dev/binderfs/test_device.")
        .get_matches();

    env_logger::init();

    let binderfs_path = Path::new(DEFAULT_BINDERFS_PATH);
    let control_path = Path::new(DEFAULT_BINDER_CONTROL_PATH);

    // Create binder control path if it doesn't exist.
    if !binderfs_path.exists() {
        match std::fs::create_dir_all(binderfs_path) {
            Ok(()) => log_ok(&format!(
                "BinderFS root directory created at {}.",
                binderfs_path.display()
            )),
            Err(err) => log_err(&format!(
                "Failed to create {}\n{}",
                binderfs_path.display(),
                err
            )),
        }
    } else {
        log_ok(&format!("{} already exists", binderfs_path.display()));
    }

    // Check if binder control path is a directory.
    if !binderfs_path.is_dir() {
        log_err(&format!("{} is not a directory", binderfs_path.display()));
    }

    // Mount binderfs if it is not mounted at the exact target.
    match is_mounted(binderfs_path) {
        Ok(true) => log_ok(&format!(
            "BinderFS is already mounted on {}",
            binderfs_path.display()
        )),
        Ok(false) => {
            let status = Command::new("mount")
                .arg("-t")
                .arg("binder")
                .arg("binder")
                .arg(binderfs_path)
                .status();
            match status {
                Ok(s) if s.success() => {
                    log_ok(&format!("BinderFS mounted at {}.", binderfs_path.display()))
                }
                Ok(s) => log_err(&format!(
                    "mount(8) exited with {} while mounting binderfs at {}",
                    s,
                    binderfs_path.display()
                )),
                Err(err) => log_err(&format!("Failed to spawn mount(8): {err}")),
            }
        }
        Err(err) => log_err(&format!("Failed to read /proc/mounts: {err}")),
    }

    // Add binder device.
    let device_name = app.get_one::<String>("device_name").unwrap();

    match binderfs::add_device(control_path, device_name) {
        Ok((_, _)) => log_ok(&format!(
            "New binder device allocated:\n\t- Device name: {device_name}\n\t- Accessible path: /dev/binderfs/{device_name}"
        )),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            log_ok(&format!("Device {device_name} already exists"));
        }
        Err(err) => log_err(&format!("Failed to allocate new binder device\n{err}")),
    }

    let device_path = binderfs_path.join(device_name);
    let mut perms = match std::fs::metadata(&device_path) {
        Ok(m) => m.permissions(),
        Err(err) => log_err(&format!(
            "Failed to read metadata of {}: {}",
            device_path.display(),
            err
        )),
    };
    perms.set_mode(0o666);
    match std::fs::set_permissions(&device_path, perms) {
        Ok(()) => log_ok(&format!(
            "Permission set to 0666 for {}",
            device_path.display()
        )),
        Err(err) => log_err(&format!(
            "Failed to change the permission of device path({}) to 0666\n{}",
            device_path.display(),
            err
        )),
    }

    let symlink_target = binderfs_path.join(device_name);
    let symlink_path = Path::new("/dev").join(device_name);
    match symlink(&symlink_target, &symlink_path) {
        Ok(()) => log_ok(&format!(
            "Symlink created from {} to {}",
            symlink_target.display(),
            symlink_path.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            log_ok(&format!(
                "Symlink {} already exists",
                symlink_path.display()
            ));
        }
        Err(err) => log_err(&format!(
            "Failed to create a symlink from {} to {}\n{}",
            symlink_target.display(),
            symlink_path.display(),
            err
        )),
    }

    println!("\nSummary:");
    println!(
        "The binder device '{device_name}' has been successfully created \
        and is accessible at /dev/binderfs/{device_name} with full permissions (read/write by all users). \
        This setup facilitates IPC mechanisms within the Linux kernel.\n"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `"binder"` substring matches `hwbinder` / `vndbinder` mounts — must reject.
    #[test]
    fn other_binderfs_mount_does_not_match() {
        let mounts = "\
binder /dev/hwbinder binder rw,relatime 0 0
binder /dev/vndbinder binder rw,relatime 0 0
proc /proc proc rw 0 0
";
        assert!(!mounts_text_contains_target(
            mounts,
            Path::new("/dev/binderfs")
        ));
    }

    #[test]
    fn exact_target_match_returns_true() {
        let mounts = "\
binder /dev/binderfs binder rw,relatime 0 0
tmpfs /tmp tmpfs rw,nosuid,nodev 0 0
";
        assert!(mounts_text_contains_target(
            mounts,
            Path::new("/dev/binderfs")
        ));
    }

    /// Substring trap: the path `/dev/binderfs` appears as a *prefix*
    /// in `/dev/binderfs2`. Exact-match parsing must distinguish them.
    #[test]
    fn substring_prefix_does_not_match() {
        let mounts = "binder /dev/binderfs2 binder rw 0 0\n";
        assert!(!mounts_text_contains_target(
            mounts,
            Path::new("/dev/binderfs")
        ));
    }

    /// Empty `/proc/mounts` and malformed lines must not panic the parser.
    #[test]
    fn empty_and_malformed_lines_are_ignored() {
        let mounts = "\n   \nbinder\n";
        assert!(!mounts_text_contains_target(
            mounts,
            Path::new("/dev/binderfs")
        ));
    }
}

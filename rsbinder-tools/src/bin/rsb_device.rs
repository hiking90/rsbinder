// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};
use std::path::Path;

use anstyle::*;
use rsbinder::*;
use rsbinder_tools::nss::{gid_for_group, uid_for_owner, NssError, MAX_ID};

/// Exact target match: a substring match on "binder" also hits `hwbinder` / `vndbinder` mounts.
fn is_mounted(binderfs_path: &Path) -> std::io::Result<bool> {
    let mounts = std::fs::read_to_string("/proc/mounts")?;
    Ok(mounts_text_contains_target(&mounts, binderfs_path))
}

/// mount(2) directly: no `$PATH` lookup of mount(8) as root.
#[cfg(any(target_os = "linux", target_os = "android"))]
fn mount_binderfs(target: &Path) -> std::io::Result<()> {
    use rustix::mount::{mount, MountFlags};
    mount(
        "binder",
        target,
        "binder",
        MountFlags::empty(),
        None::<&std::ffi::CStr>,
    )?;
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "android")))]
fn mount_binderfs(_target: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "binderfs needs a Linux kernel",
    ))
}

/// Split from `is_mounted` so tests need no `/proc`.
fn mounts_text_contains_target(mounts: &str, target: &Path) -> bool {
    mounts.lines().any(|line| {
        line.split_whitespace()
            .nth(1)
            .is_some_and(|t| Path::new(t) == target)
    })
}

/// Root-only: binder has no in-kernel access control, so the node's mode is the only gate.
const DEFAULT_DEVICE_MODE: u32 = 0o600;

/// Rejects set-uid/set-gid/sticky: meaningless on a char device, and the shape of a typo.
fn parse_mode(spec: &str) -> std::result::Result<u32, String> {
    let digits = spec
        .strip_prefix("0o")
        .or_else(|| spec.strip_prefix("0O"))
        .unwrap_or(spec);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() && b < b'8') {
        return Err(format!("mode must be octal digits, got {spec:?}"));
    }
    let mode = u32::from_str_radix(digits, 8).map_err(|e| format!("invalid mode {spec:?}: {e}"))?;
    if mode > 0o777 {
        return Err(format!(
            "mode {spec:?} out of range: set-uid/set-gid/sticky bits are not \
             accepted on a device node (max 777)"
        ));
    }
    Ok(mode)
}

/// A name the database maps to `(uid_t)-1`, which chown(2) reads as "leave unchanged".
fn chown_id(kind: &str, spec: &str, id: u32) -> std::result::Result<u32, String> {
    if id > MAX_ID {
        return Err(format!(
            "{spec:?} has {kind} {id}, which chown(2) reads as \"leave unchanged\""
        ));
    }
    Ok(id)
}

/// The mode to set before a chown, if any: `old & new` grants nothing that `old` does not.
fn mode_before_chown(old: u32, new: u32) -> Option<u32> {
    let old = old & 0o7777;
    let narrowed = old & new;
    (narrowed != old).then_some(narrowed)
}

/// `"alice (uid 1000)"`, or `"uid 1000"` when the spec was the number itself.
fn describe_id(kind: &str, spec: &str, id: u32) -> String {
    if spec == id.to_string() {
        format!("{kind} {id}")
    } else {
        format!("{spec} ({kind} {id})")
    }
}

/// A failed name lookup, with the way around a name the database does not have.
fn lookup_error(err: NssError, kind: &str, id: &str) -> String {
    match err {
        NssError::OutOfRange { .. } => err.to_string(),
        _ => {
            format!("{err}\nA {kind} absent from the {kind} database can be given by numeric {id}.")
        }
    }
}

/// Who can open the node for summary purposes.
#[derive(Debug, PartialEq, Eq)]
enum Openers {
    RootOnly,
    OwnerAndRoot,
    GroupOrOther,
}

/// rsbinder opens the node `O_RDWR`: the owner needs `r` and `w`, root needs neither.
fn openers(mode: u32, uid: u32) -> Openers {
    if mode & 0o077 != 0 {
        Openers::GroupOrOther
    } else if uid != 0 && mode & 0o600 == 0o600 {
        Openers::OwnerAndRoot
    } else {
        Openers::RootOnly
    }
}

fn set_mode(path: &Path, mode: u32) {
    if let Err(err) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)) {
        fail(&format!(
            "Failed to change the permission of device path({}) to {mode:04o}\n{err}",
            path.display()
        ));
    }
}

fn log_ok(msg: &str) {
    let style = Style::new().fg_color(Some(AnsiColor::Green.into())).bold();
    println!("[{}OK{}] {}", style.render(), style.render_reset(), msg);
}

fn fail(msg: &str) -> ! {
    eprintln!("rsb_device: {msg}");
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
        .arg(clap::Arg::new("mode")
             .short('m')
             .long("mode")
             .value_name("MODE")
             .help("Octal permission bits for the device node (default: 0600, root only)"))
        .arg(clap::Arg::new("owner")
             .short('o')
             .long("owner")
             .value_name("USER")
             .help("User name or numeric uid to own the device node (default: unchanged)"))
        .arg(clap::Arg::new("group")
             .short('g')
             .long("group")
             .value_name("GROUP")
             .help("Group name or numeric gid to own the device node (default: unchanged)"))
        .after_help("Examples:\n    \
            Create a root-only binder device (default 0600):\n    \
            $ sudo ./rsb_device mybinder\n\n    \
            Grant a group access, the /dev/kvm model:\n    \
            $ sudo groupadd -f binder\n    \
            $ sudo ./rsb_device binder --group binder --mode 0660\n    \
            $ sudo usermod -aG binder \"$USER\"\n\n    \
            Give one user a device of their own (a digits-only USER or GROUP is\n    \
            an id and is never looked up):\n    \
            $ sudo ./rsb_device binder-alice --owner alice\n    \
            ExecStartPre=+/usr/bin/rsb_device binder-%i --owner %i\n    \
            (the second in a user@.service drop-in, where %i is the uid)\n\n    \
            Remove a device with rm. A new device of the same name starts with no\n    \
            service manager, so a service manager running as another uid can register:\n    \
            $ sudo rm /dev/binderfs/binder-alice\n\n    \
            The device node's mode is the only gate on who can speak binder at\n    \
            all -- binder itself has no in-kernel access control. rsb_hub layers\n    \
            per-service policy on top; see its --config option.")
        .get_matches();

    rsbinder_tools::logging::init_command();

    // Validate every argument before the mount below, so a rejection leaves nothing behind.
    let mode = match app.get_one::<String>("mode") {
        Some(spec) => match parse_mode(spec) {
            Ok(mode) => mode,
            Err(err) => fail(&err),
        },
        None => DEFAULT_DEVICE_MODE,
    };
    let owner = match app.get_one::<String>("owner") {
        Some(spec) => match uid_for_owner(spec) {
            Ok(uid) => match chown_id("uid", spec, uid) {
                Ok(uid) => Some((spec.clone(), uid)),
                Err(err) => fail(&err),
            },
            Err(err) => fail(&lookup_error(err, "user", "uid")),
        },
        None => None,
    };
    let gid = match app.get_one::<String>("group") {
        Some(spec) => match gid_for_group(spec) {
            Ok(gid) => match chown_id("gid", spec, gid) {
                Ok(gid) => Some((spec.clone(), gid)),
                Err(err) => fail(&err),
            },
            Err(err) => fail(&lookup_error(err, "group", "gid")),
        },
        None => None,
    };

    let device_name = app
        .get_one::<String>("device_name")
        .expect("clap enforces the required device_name");

    // Runs as root and is joined into paths below: one component, within BINDERFS_MAX_NAME (255).
    if device_name.is_empty()
        || device_name == "."
        || device_name == ".."
        || device_name.contains('/')
        || device_name.contains('\0')
        || device_name.len() > 255
    {
        fail(&format!("Invalid binder device name: {device_name:?}"));
    }
    // binderfs root entries: add_device reports EEXIST and the chown/chmod would hit them.
    if matches!(
        device_name.as_str(),
        "binder-control" | "features" | "binder_logs"
    ) {
        fail(&format!("{device_name:?} is reserved by binderfs"));
    }

    let binderfs_path = Path::new(DEFAULT_BINDERFS_PATH);
    let control_path = Path::new(DEFAULT_BINDER_CONTROL_PATH);

    // Create binder control path if it doesn't exist.
    if !binderfs_path.exists() {
        match std::fs::create_dir_all(binderfs_path) {
            Ok(()) => log_ok(&format!(
                "BinderFS root directory created at {}.",
                binderfs_path.display()
            )),
            Err(err) => fail(&format!(
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
        fail(&format!("{} is not a directory", binderfs_path.display()));
    }

    // Mount binderfs if it is not mounted at the exact target.
    match is_mounted(binderfs_path) {
        Ok(true) => log_ok(&format!(
            "BinderFS is already mounted on {}",
            binderfs_path.display()
        )),
        Ok(false) => match mount_binderfs(binderfs_path) {
            Ok(()) => log_ok(&format!("BinderFS mounted at {}.", binderfs_path.display())),
            Err(err) => fail(&format!(
                "Failed to mount binderfs at {}: {err}",
                binderfs_path.display()
            )),
        },
        Err(err) => fail(&format!("Failed to read /proc/mounts: {err}")),
    }

    // Add binder device.
    let existed = match binderfs::add_device(control_path, device_name) {
        Ok((_, _)) => {
            log_ok(&format!(
                "New binder device allocated:\n\t- Device name: {device_name}\n\t- Accessible path: /dev/binderfs/{device_name}"
            ));
            false
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            log_ok(&format!("Device {device_name} already exists"));
            true
        }
        Err(err) => fail(&format!(
            "Failed to allocate new binder device via {}\n{err}",
            control_path.display()
        )),
    };

    let device_path = binderfs_path.join(device_name);
    let meta = match std::fs::metadata(&device_path) {
        Ok(m) => m,
        Err(err) => fail(&format!(
            "Failed to read metadata of {}: {}",
            device_path.display(),
            err
        )),
    };
    let new_uid = owner.as_ref().map(|(_, uid)| *uid);
    let new_gid = gid.as_ref().map(|(_, gid)| *gid);
    // binder keeps the first context manager's euid for the device's life; chown closes no fd.
    if existed && new_uid.is_some_and(|uid| uid != meta.uid()) {
        eprintln!(
            "rsb_device: warning: {path} was owned by uid {old}. The kernel lets\n  \
             only the uid of the first service manager that ran on this device\n  \
             register one again, and descriptors opened before this change stay\n  \
             open. If the device was in use, remove it and run this again:\n      \
             $ sudo rm {path}",
            path = device_path.display(),
            old = meta.uid()
        );
    }
    // chmod(old & new) -> chown -> chmod(new): no step grants what neither old nor new grants.
    if new_uid.is_some() || new_gid.is_some() {
        let narrowed = mode_before_chown(meta.mode(), mode);
        if let Some(narrowed) = narrowed {
            set_mode(&device_path, narrowed);
        }
        let target = owner
            .iter()
            .map(|(spec, uid)| format!("owner {}", describe_id("uid", spec, *uid)))
            .chain(
                gid.iter()
                    .map(|(spec, gid)| format!("group {}", describe_id("gid", spec, *gid))),
            )
            .collect::<Vec<_>>()
            .join(", ");
        match std::os::unix::fs::chown(&device_path, new_uid, new_gid) {
            Ok(()) => log_ok(&format!("Set {target} for {}", device_path.display())),
            Err(err) => fail(&format!(
                "Failed to set {target} for {path}\n{err}{left}",
                path = device_path.display(),
                left = narrowed.map_or(String::new(), |m| format!(
                    "\n{} was left at mode {m:04o} under its previous owner and group",
                    device_path.display()
                ))
            )),
        }
    }

    set_mode(&device_path, mode);
    log_ok(&format!(
        "Permission set to {mode:04o} for {}",
        device_path.display()
    ));

    let symlink_target = binderfs_path.join(device_name);
    let symlink_path = Path::new("/dev").join(device_name);
    match symlink(&symlink_target, &symlink_path) {
        Ok(()) => log_ok(&format!(
            "Symlink created from {} to {}",
            symlink_target.display(),
            symlink_path.display()
        )),
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::read_link(&symlink_path) {
                Ok(t) if t == symlink_target => log_ok(&format!(
                    "Symlink {} already exists",
                    symlink_path.display()
                )),
                _ => eprintln!(
                    "rsb_device: warning: {} exists and is not a symlink to {}; left unchanged",
                    symlink_path.display(),
                    symlink_target.display()
                ),
            }
        }
        Err(err) => fail(&format!(
            "Failed to create a symlink from {} to {}\n{}",
            symlink_target.display(),
            symlink_path.display(),
            err
        )),
    }

    println!("\nSummary:");
    println!(
        "The binder device '{device_name}' has been created at \
        /dev/binderfs/{device_name} with mode {mode:04o}."
    );
    let final_uid = new_uid.unwrap_or(meta.uid());
    match openers(mode, final_uid) {
        Openers::RootOnly => {
            let argv0 = std::env::args()
                .next()
                .unwrap_or_else(|| "rsb_device".to_owned());
            println!(
                "Only root can open it. Grant access deliberately, e.g.\n    \
                $ sudo {argv0} {device_name} --group binder --mode 0660\n    \
                $ sudo {argv0} {device_name} --owner \"$USER\""
            );
        }
        Openers::OwnerAndRoot => {
            let who = match &owner {
                Some((spec, uid)) => describe_id("uid", spec, *uid),
                None => format!("uid {final_uid}"),
            };
            println!("Only {who} and root can open it.");
        }
        Openers::GroupOrOther => {}
    }
    println!();

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

    /// `/dev/binderfs` is a prefix of `/dev/binderfs2`.
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

    #[test]
    fn parse_mode_accepts_octal_spellings() {
        assert_eq!(parse_mode("660").unwrap(), 0o660);
        assert_eq!(parse_mode("0660").unwrap(), 0o660);
        assert_eq!(parse_mode("0o660").unwrap(), 0o660);
        assert_eq!(parse_mode("600").unwrap(), DEFAULT_DEVICE_MODE);
        assert_eq!(parse_mode("0").unwrap(), 0);
        assert_eq!(parse_mode("777").unwrap(), 0o777);
    }

    /// `8`/`9` are decimal but not octal digits, so the digit class is checked explicitly.
    #[test]
    fn parse_mode_rejects_non_octal() {
        assert!(parse_mode("").is_err());
        assert!(parse_mode("0o").is_err());
        assert!(parse_mode("668").is_err());
        assert!(parse_mode("6a6").is_err());
        assert!(parse_mode("-660").is_err());
        assert!(parse_mode(" 660").is_err());
    }

    /// `4660` for `0660` is the typo this guards against.
    #[test]
    fn parse_mode_rejects_special_bits() {
        assert!(parse_mode("4660").is_err());
        assert!(parse_mode("2660").is_err());
        assert!(parse_mode("1660").is_err());
        assert!(parse_mode("7777").is_err());
    }

    #[test]
    fn chown_id_rejects_the_leave_unchanged_value() {
        assert_eq!(chown_id("uid", "1000", 1000), Ok(1000));
        assert_eq!(chown_id("gid", "0", 0), Ok(0));
        assert_eq!(chown_id("gid", "nfsnobody", MAX_ID), Ok(MAX_ID));
        assert!(chown_id("uid", "broken", u32::MAX).is_err());
        assert!(chown_id("gid", "broken", u32::MAX).is_err());
        // Digits stop at the lookup, with the one message for any number past `MAX_ID`.
        for spec in ["4294967295", "4294967296"] {
            let err = gid_for_group(spec).expect_err("past MAX_ID");
            assert!(matches!(err, NssError::OutOfRange { .. }), "{spec}");
        }
    }

    /// Before the chown the node is `old & new` under the old ids, after it a subset of `new`.
    #[test]
    fn mode_before_chown_never_widens() {
        const S_IFCHR: u32 = 0o020000;
        assert_eq!(mode_before_chown(S_IFCHR | 0o600, 0o660), None);
        assert_eq!(mode_before_chown(S_IFCHR | 0o600, 0o600), None);
        assert_eq!(mode_before_chown(S_IFCHR | 0o660, 0o600), Some(0o600));
        assert_eq!(mode_before_chown(S_IFCHR | 0o666, 0o640), Some(0o640));
        assert_eq!(mode_before_chown(S_IFCHR | 0o640, 0o604), Some(0o600));
        for old in 0..=0o777 {
            for new in [0o000, 0o600, 0o640, 0o660, 0o604, 0o777] {
                let before = mode_before_chown(S_IFCHR | old, new).unwrap_or(old);
                assert_eq!(before & !old, 0, "old {old:o} new {new:o}");
                assert_eq!(before & !new, 0, "old {old:o} new {new:o}");
            }
        }
    }

    /// An owner without both `r` and `w` cannot open the node `O_RDWR`; root always can.
    #[test]
    fn openers_need_read_and_write_for_the_owner() {
        assert_eq!(openers(0o600, 0), Openers::RootOnly);
        assert_eq!(openers(0o600, 1000), Openers::OwnerAndRoot);
        assert_eq!(openers(0o700, 1000), Openers::OwnerAndRoot);
        assert_eq!(openers(0o400, 1000), Openers::RootOnly);
        assert_eq!(openers(0o200, 1000), Openers::RootOnly);
        assert_eq!(openers(0o000, 1000), Openers::RootOnly);
        assert_eq!(openers(0o660, 1000), Openers::GroupOrOther);
        assert_eq!(openers(0o604, 0), Openers::GroupOrOther);
    }

    #[test]
    fn describe_id_drops_a_spec_that_is_the_number() {
        assert_eq!(describe_id("uid", "1000", 1000), "uid 1000");
        assert_eq!(describe_id("uid", "alice", 1000), "alice (uid 1000)");
        assert_eq!(describe_id("gid", "01000", 1000), "01000 (gid 1000)");
    }

    #[test]
    fn lookup_error_names_an_out_of_range_number_as_one() {
        let err = uid_for_owner("4294967295").expect_err("past MAX_ID");
        assert_eq!(
            lookup_error(err, "user", "uid"),
            "uid 4294967295 is out of range: a uid is at most 4294967294"
        );
        let err = gid_for_group("rsbinder-no-such-group-ffffffff").expect_err("no such group");
        assert!(lookup_error(err, "group", "gid").contains("numeric gid"));
    }
}

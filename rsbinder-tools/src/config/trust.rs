// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Is this configuration safe to obey?
//!
//! The configuration decides who may register a service and, with a
//! `start` entry, what command runs when a lookup misses — a command that
//! runs with rsb_hub's privileges and is triggered by any client allowed to
//! look the name up. A file anyone can edit is therefore a file anyone can
//! use to run code as root, and a group-writable policy is one anyone in
//! that group can use to grant themselves registration.
//!
//! So the ownership and mode of the configuration are checked before it is
//! read, and a failure stops the process; a failed SIGHUP reload keeps the
//! policy already in force. Every `exec` program the configuration names is
//! checked when it is loaded, with the same outcome, and again right before
//! each start, where a failure refuses that start: a program the check
//! passed at load is not run once a later change makes it fail. This is the
//! same discipline sudo, ssh and cron apply to their own configuration, for
//! the same reason.
//!
//! The attacker is a non-root uid that can write some component on the
//! lookup path — a directory above the configuration, a symlink on the way,
//! or an entry in the configuration directory. Against that, the walk
//! guarantees one thing: every inode it holds, from `/` down to the final
//! file — symlink targets and the directories holding the symlinks included —
//! is owned by root or by rsb_hub's uid, is neither group- nor
//! world-writable, and is read through the very descriptor that was checked.
//! Each component is opened with `O_NOFOLLOW` relative to the directory
//! already held, so a rename, unlink or symlink swap after the check cannot
//! change what is read, and the kernel never resolves a symlink on rsb_hub's
//! behalf: a symlink is read with `readlinkat` and its target walked the same
//! way, an absolute target restarting from the walk's root (`/` in
//! production). There is no sticky-directory exception, so anything under a
//! world-writable directory such as `/tmp` is refused. Not guaranteed: what
//! someone who can manipulate mounts (`CAP_SYS_ADMIN`, so root-equivalent)
//! can do, what root itself does, and the contents of the files.

use std::ffi::{OsStr, OsString};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use rustix::fs::{fstat, openat, readlinkat, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

/// Why a configuration path cannot be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrustProblem {
    /// Any user can edit it.
    WorldWritable,
    /// Any member of its group can edit it.
    GroupWritable,
    /// Owned by someone who is neither root nor us, so that someone can
    /// edit it — and change its mode back whenever they like.
    ForeignOwner(u32),
}

impl std::fmt::Display for TrustProblem {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TrustProblem::WorldWritable => f.write_str("world-writable"),
            TrustProblem::GroupWritable => f.write_str("group-writable"),
            TrustProblem::ForeignOwner(uid) => {
                write!(
                    f,
                    "owned by uid {uid}, which is neither root nor this process"
                )
            }
        }
    }
}

/// The pure decision, split from the `stat` so every case is testable
/// without creating files as other users.
///
/// Writability is checked before ownership because it is the sharper
/// problem: a root-owned but world-writable file is worse than a
/// correctly-moded file owned by an unexpected user.
pub fn trust_problem(mode: u32, owner_uid: u32, our_uid: u32) -> Option<TrustProblem> {
    if mode & 0o002 != 0 {
        return Some(TrustProblem::WorldWritable);
    }
    if mode & 0o020 != 0 {
        return Some(TrustProblem::GroupWritable);
    }
    if owner_uid != 0 && owner_uid != our_uid {
        return Some(TrustProblem::ForeignOwner(owner_uid));
    }
    None
}

/// A path that failed the check.
#[derive(Debug, Clone)]
pub struct Untrusted {
    /// The offending path.
    pub path: PathBuf,
    /// What is wrong with it.
    pub problem: TrustProblem,
}

/// A directory we hold, and the path it is shown as in errors.
#[derive(Clone, Copy)]
pub(crate) struct At<'a> {
    pub fd: BorrowedFd<'a>,
    pub shown: &'a Path,
}

/// The inode a walk ended on: held open, so what is read is what was checked.
#[derive(Debug)]
pub(crate) struct Trusted {
    pub fd: OwnedFd,
    pub kind: FileType,
    pub shown: PathBuf,
}

impl Trusted {
    pub(crate) fn at(&self) -> At<'_> {
        At {
            fd: self.fd.as_fd(),
            shown: &self.shown,
        }
    }
}

/// Linux `MAXSYMLINKS`: past this many, the kernel's own lookup fails with `ELOOP`.
const MAX_SYMLINKS: usize = 40;

#[cfg(any(target_os = "linux", target_os = "android"))]
const INTERMEDIATE: OFlags = OFlags::PATH;
/// No `O_PATH` here, so a directory on the way needs `r` as well as `x`.
#[cfg(not(any(target_os = "linux", target_os = "android")))]
const INTERMEDIATE: OFlags = OFlags::RDONLY;

/// What the fd a walk ends on is for, which decides how its last component is opened.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Use {
    /// Read or listed: the configuration file or directory.
    Read,
    /// Only `fstat`ed, so `O_PATH` where there is one: an `exec` program may lack `r`.
    Stat,
}

/// Open the directory a walk starts from.
pub(crate) fn open_root(path: &Path) -> std::io::Result<OwnedFd> {
    let flags = INTERMEDIATE | OFlags::DIRECTORY | OFlags::CLOEXEC;
    Ok(rustix::fs::open(path, flags, Mode::empty())?)
}

/// `st_mode` is `u16` on darwin and `u32` elsewhere; the check wants the wider one.
#[allow(
    clippy::useless_conversion,
    reason = "`st_mode` is already `u32` off darwin"
)]
fn mode_bits(st: &Stat) -> u32 {
    u32::from(st.st_mode)
}

fn push_reversed(todo: &mut Vec<OsString>, path: &Path) {
    let parts = path.components().filter_map(|c| match c {
        Component::Normal(name) => Some(name.to_owned()),
        Component::ParentDir => Some(OsString::from("..")),
        Component::RootDir | Component::CurDir | Component::Prefix(_) => None,
    });
    let at = todo.len();
    todo.extend(parts);
    todo[at..].reverse();
}

struct Walk<'a> {
    root: At<'a>,
    cur: OwnedFd,
    shown: PathBuf,
    todo: Vec<OsString>,
    hops: usize,
}

impl Walk<'_> {
    /// Queue the target of the symlink `name` in `cur`; an absolute one restarts from the root.
    fn follow(&mut self, name: &OsStr) -> std::io::Result<()> {
        self.hops += 1;
        if self.hops > MAX_SYMLINKS {
            return Err(Errno::LOOP.into());
        }
        let target = readlinkat(&self.cur, name, Vec::new())?;
        let target = PathBuf::from(OsString::from_vec(target.into_bytes()));
        if target.is_absolute() {
            self.cur = self.root.fd.try_clone_to_owned()?;
            self.shown = self.root.shown.to_owned();
        }
        push_reversed(&mut self.todo, &target);
        Ok(())
    }
}

/// Walk `rel` from `start` one component per `openat`; absolute symlink targets restart at `root`.
pub(crate) fn check_from(
    root: At<'_>,
    start: At<'_>,
    rel: &Path,
    our_uid: u32,
    last: Use,
) -> std::io::Result<Result<Trusted, Untrusted>> {
    let mut walk = Walk {
        root,
        cur: start.fd.try_clone_to_owned()?,
        shown: start.shown.to_owned(),
        todo: Vec::new(),
        hops: 0,
    };
    let mut st = fstat(&walk.cur)?;
    if let Some(problem) = trust_problem(mode_bits(&st), st.st_uid, our_uid) {
        return Ok(Err(Untrusted {
            path: walk.shown,
            problem,
        }));
    }
    push_reversed(&mut walk.todo, rel);
    let mut readable = false;
    while let Some(name) = walk.todo.pop() {
        // A last component that is read or listed cannot be an `O_PATH` fd.
        let access = if walk.todo.is_empty() && last == Use::Read {
            OFlags::RDONLY
        } else {
            INTERMEDIATE
        };
        // `NONBLOCK`: a FIFO named `x.toml` must not stall the open until a writer appears.
        let flags = access | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOCTTY;
        let child = match openat(&walk.cur, &name, flags, Mode::empty()) {
            Ok(fd) => fd,
            // `O_RDONLY | O_NOFOLLOW` on a symlink; `O_PATH` instead yields an fd to the link.
            Err(e) if e == Errno::LOOP => {
                walk.follow(&name)?;
                readable = false;
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        let child_st = fstat(&child)?;
        if FileType::from_raw_mode(child_st.st_mode) == FileType::Symlink {
            walk.follow(&name)?;
            readable = false;
            continue;
        }
        readable = access == OFlags::RDONLY;
        if let Some(problem) = trust_problem(mode_bits(&child_st), child_st.st_uid, our_uid) {
            return Ok(Err(Untrusted {
                path: walk.shown.join(&name),
                problem,
            }));
        }
        if name == ".." {
            walk.shown.pop();
        } else {
            walk.shown.push(&name);
        }
        walk.cur = child;
        st = child_st;
    }
    if last == Use::Read && !readable {
        // Ended on the start or a symlink's directory (`/`, `.`): `.` reopens that inode readable.
        let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
        walk.cur = openat(&walk.cur, ".", flags, Mode::empty())?;
        st = fstat(&walk.cur)?;
    }
    Ok(Ok(Trusted {
        fd: walk.cur,
        kind: FileType::from_raw_mode(st.st_mode),
        shown: walk.shown,
    }))
}

/// Walk the absolute `exec` program from `root`, the check the configuration itself gets.
pub(crate) fn check_program(
    root: At<'_>,
    program: &Path,
    our_uid: u32,
) -> std::io::Result<Result<(), Untrusted>> {
    let rel = program.strip_prefix("/").unwrap_or(program);
    Ok(check_from(root, root, rel, our_uid, Use::Stat)?.map(drop))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{set_permissions, Permissions};
    use std::io::Read;
    use std::os::unix::fs::{symlink, MetadataExt, PermissionsExt};

    const ROOT: u32 = 0;
    const US: u32 = 1000;
    const THEM: u32 = 1001;

    #[test]
    fn a_root_owned_private_path_is_trusted() {
        assert_eq!(trust_problem(0o755, ROOT, US), None);
        assert_eq!(trust_problem(0o644, ROOT, US), None);
        assert_eq!(trust_problem(0o600, ROOT, US), None);
    }

    /// Unprivileged runs are normal in development; a file we own nobody else can change.
    #[test]
    fn a_path_we_own_is_trusted() {
        assert_eq!(trust_problem(0o755, US, US), None);
    }

    #[test]
    fn writable_by_others_is_rejected() {
        assert_eq!(
            trust_problem(0o777, ROOT, US),
            Some(TrustProblem::WorldWritable)
        );
        assert_eq!(
            trust_problem(0o666, ROOT, US),
            Some(TrustProblem::WorldWritable)
        );
        assert_eq!(
            trust_problem(0o775, ROOT, US),
            Some(TrustProblem::GroupWritable)
        );
        assert_eq!(
            trust_problem(0o664, ROOT, US),
            Some(TrustProblem::GroupWritable)
        );
    }

    /// The configuration is not a secret; only writability decides what runs.
    #[test]
    fn readable_by_others_is_fine() {
        assert_eq!(trust_problem(0o644, ROOT, US), None);
        assert_eq!(trust_problem(0o755, ROOT, US), None);
    }

    #[test]
    fn a_path_someone_else_owns_is_rejected() {
        assert_eq!(
            trust_problem(0o600, THEM, US),
            Some(TrustProblem::ForeignOwner(THEM))
        );
    }

    /// Writability outranks ownership: a file anyone can edit is the worse finding.
    #[test]
    fn writability_is_reported_before_ownership() {
        assert_eq!(
            trust_problem(0o666, THEM, US),
            Some(TrustProblem::WorldWritable)
        );
    }

    /// A fresh 0755 directory held as the walk's root; nothing above it is ever looked at.
    struct Sandbox {
        root: OwnedFd,
        base: PathBuf,
    }

    impl Sandbox {
        fn new(tag: &str) -> Self {
            let base = std::env::temp_dir().join(format!("rsb-trust-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            chmod(&base, 0o755);
            let root = open_root(&base).unwrap();
            Sandbox { root, base }
        }

        fn at(&self) -> At<'_> {
            At {
                fd: self.root.as_fd(),
                shown: &self.base,
            }
        }

        /// Explicit modes: under umask 0002 the defaults are group-writable and fail the check.
        fn dir(&self, rel: &str) -> PathBuf {
            let path = self.base.join(rel);
            std::fs::create_dir_all(&path).unwrap();
            // Intermediate directories get the umask default too, so chmod every level.
            for made in path.ancestors().take_while(|p| *p != self.base) {
                chmod(made, 0o755);
            }
            path
        }

        fn file(&self, rel: &str, text: &str) -> PathBuf {
            let path = self.base.join(rel);
            std::fs::write(&path, text).unwrap();
            chmod(&path, 0o644);
            path
        }

        fn check(&self, rel: &str) -> std::io::Result<Result<Trusted, Untrusted>> {
            let us = rustix::process::getuid().as_raw();
            check_from(self.at(), self.at(), Path::new(rel), us, Use::Read)
        }

        fn trusted(&self, rel: &str) -> Trusted {
            self.check(rel).unwrap().expect("must be trusted")
        }

        fn untrusted(&self, rel: &str) -> Untrusted {
            self.check(rel).unwrap().expect_err("must be rejected")
        }
    }

    impl Drop for Sandbox {
        fn drop(&mut self) {
            chmod(&self.base, 0o755);
            let _ = std::fs::remove_dir_all(&self.base);
        }
    }

    fn chmod(path: &Path, mode: u32) {
        set_permissions(path, Permissions::from_mode(mode)).unwrap();
    }

    fn ino(path: &Path) -> u64 {
        std::fs::metadata(path).unwrap().ino()
    }

    fn held_ino(t: &Trusted) -> u64 {
        fstat(&t.fd).unwrap().st_ino
    }

    fn read_held(t: Trusted) -> String {
        let mut text = String::new();
        std::fs::File::from(t.fd).read_to_string(&mut text).unwrap();
        text
    }

    fn held_names(t: &Trusted) -> Vec<String> {
        let mut names: Vec<String> = rustix::fs::Dir::read_from(&t.fd)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_str().unwrap().to_owned())
            .filter(|n| n != "." && n != "..")
            .collect();
        names.sort();
        names
    }

    #[test]
    fn a_writable_parent_directory_is_rejected() {
        let sb = Sandbox::new("parent");
        let d = sb.dir("d");
        let file = sb.file("d/hub.toml", "");

        let t = sb.trusted("d/hub.toml");
        assert_eq!(t.kind, FileType::RegularFile);
        assert_eq!(t.shown, file);

        chmod(&d, 0o775);
        let bad = sb.untrusted("d/hub.toml");
        assert_eq!(bad.problem, TrustProblem::GroupWritable);
        assert_eq!(bad.path, d);
    }

    /// The held fd reads the checked inode; parse.rs tests catch a read that reopens by path.
    #[test]
    fn a_swapped_file_is_not_reread() {
        let sb = Sandbox::new("swap-file");
        sb.dir("d");
        sb.file("d/hub.toml", "A");
        sb.file("d/other.toml", "B");

        let t = sb.trusted("d/hub.toml");
        std::fs::rename(sb.base.join("d/other.toml"), sb.base.join("d/hub.toml")).unwrap();
        assert_eq!(read_held(t), "A");
    }

    /// The directory holding a symlink is checked, and so is every directory under its target.
    #[test]
    fn a_writable_directory_holding_a_symlink_is_rejected() {
        let sb = Sandbox::new("link");
        let outer = sb.dir("outer");
        let loose = sb.dir("loose/real");
        let real = sb.file("loose/real/hub.toml", "");
        symlink("../loose/real", outer.join("hub.d")).unwrap();

        let t = sb.trusted("outer/hub.d/hub.toml");
        assert_eq!(held_ino(&t), ino(&real));
        assert_eq!(t.shown, real);

        chmod(&outer, 0o775);
        let bad = sb.untrusted("outer/hub.d/hub.toml");
        assert_eq!(bad.problem, TrustProblem::GroupWritable);
        assert_eq!(bad.path, outer);

        chmod(&outer, 0o755);
        chmod(loose.parent().unwrap(), 0o775);
        let bad = sb.untrusted("outer/hub.d/hub.toml");
        assert_eq!(bad.problem, TrustProblem::GroupWritable);
        assert_eq!(bad.path, sb.base.join("loose"));
    }

    #[test]
    fn a_swapped_symlink_does_not_change_the_file_set() {
        let sb = Sandbox::new("swap-link");
        let outer = sb.dir("outer");
        sb.dir("real1");
        sb.dir("real2");
        sb.file("real1/10.toml", "");
        sb.file("real2/99.toml", "");
        symlink("../real1", outer.join("hub.d")).unwrap();

        let t = sb.trusted("outer/hub.d");
        assert_eq!(t.kind, FileType::Directory);
        std::fs::remove_file(outer.join("hub.d")).unwrap();
        symlink("../real2", outer.join("hub.d")).unwrap();
        assert_eq!(held_names(&t), ["10.toml"]);
    }

    /// `/tmp` is sticky and root's; a config there is still one anyone could have placed.
    #[test]
    fn a_sticky_directory_is_no_exception() {
        let sb = Sandbox::new("sticky");
        let d = sb.dir("d");
        sb.file("d/hub.toml", "");
        chmod(&d, 0o1777);

        let bad = sb.untrusted("d/hub.toml");
        assert_eq!(bad.problem, TrustProblem::WorldWritable);
        assert_eq!(bad.path, d);
    }

    #[test]
    fn an_entry_replaced_by_a_symlink_after_open_is_not_followed() {
        let sb = Sandbox::new("swap-entry");
        sb.dir("d");
        let file = sb.file("d/hub.toml", "A");
        let other = sb.file("other.toml", "B");

        let t = sb.trusted("d/hub.toml");
        let before = held_ino(&t);
        std::fs::remove_file(&file).unwrap();
        symlink(&other, &file).unwrap();
        assert_eq!(held_ino(&t), before);
        assert_ne!(held_ino(&t), ino(&other));
        assert_eq!(read_held(t), "A");
    }

    /// Without the hop limit the 41-link chain resolves and the expect_err below fails.
    #[test]
    fn a_symlink_chain_is_bounded_like_the_kernel() {
        let sb = Sandbox::new("chain");
        let d = sb.dir("d");
        sb.file("d/file", "");
        for (prefix, links) in [("l", MAX_SYMLINKS), ("m", MAX_SYMLINKS + 1)] {
            for i in 0..links {
                let next = if i + 1 == links {
                    "file".to_owned()
                } else {
                    format!("{prefix}{}", i + 1)
                };
                symlink(next, d.join(format!("{prefix}{i}"))).unwrap();
            }
        }
        symlink("b", d.join("a")).unwrap();
        symlink("a", d.join("b")).unwrap();
        let eloop = Errno::LOOP.raw_os_error();

        assert_eq!(sb.trusted("d/l0").shown, d.join("file"));
        let err = sb.check("d/m0").expect_err("41 links must fail");
        assert_eq!(err.raw_os_error(), Some(eloop));
        let err = sb.check("d/a").expect_err("a loop must fail");
        assert_eq!(err.raw_os_error(), Some(eloop));
    }

    /// `..` climbs from the symlink's target, not from the symlink's own directory.
    #[test]
    fn dot_dot_after_a_symlink_resolves_against_the_target() {
        let sb = Sandbox::new("dotdot");
        let d = sb.dir("d");
        sb.dir("d/sub/inner");
        let s = sb.file("d/sub/x.toml", "S");
        sb.file("d/x.toml", "D");
        symlink("sub/inner", d.join("link")).unwrap();

        let t = sb.trusted("d/link/../x.toml");
        assert_eq!(held_ino(&t), ino(&s));
        assert_eq!(t.shown, s);
        assert_eq!(read_held(t), "S");
    }

    #[test]
    fn the_start_itself_is_checked() {
        let sb = Sandbox::new("start");
        let t = sb.trusted("");
        assert_eq!(t.kind, FileType::Directory);
        assert_eq!(t.shown, sb.base);

        chmod(&sb.base, 0o777);
        let bad = sb.untrusted("");
        assert_eq!(bad.problem, TrustProblem::WorldWritable);
        assert_eq!(bad.path, sb.base);
    }
}

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
//! read, and a failure stops the process. This is the same discipline sudo,
//! ssh and cron apply to their own configuration, for the same reason.

use std::path::{Path, PathBuf};

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

/// Check one path's ownership and mode.
pub fn check_path(path: &Path, our_uid: u32) -> std::io::Result<Option<Untrusted>> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(path)?;
    Ok(
        trust_problem(meta.mode(), meta.uid(), our_uid).map(|problem| Untrusted {
            path: path.to_owned(),
            problem,
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: u32 = 0;
    const US: u32 = 1000;
    const THEM: u32 = 1001;

    #[test]
    fn a_root_owned_private_path_is_trusted() {
        assert_eq!(trust_problem(0o755, ROOT, US), None);
        assert_eq!(trust_problem(0o644, ROOT, US), None);
        assert_eq!(trust_problem(0o600, ROOT, US), None);
    }

    /// Running unprivileged is normal in development, and a file we own is
    /// one nobody else can change.
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

    /// Readable by everyone is fine — the configuration is not a secret.
    /// It is *writability* that decides what runs.
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

    /// Writability outranks ownership: a root-owned file anyone can edit is
    /// the worse finding, and naming it as "owned by root" would be absurd.
    #[test]
    fn writability_is_reported_before_ownership() {
        assert_eq!(
            trust_problem(0o666, THEM, US),
            Some(TrustProblem::WorldWritable)
        );
    }

    #[test]
    fn check_path_reads_the_real_mode() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rsb-trust-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let us = rustix::process::getuid().as_raw();

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(check_path(&dir, us).unwrap().is_none());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let bad = check_path(&dir, us).unwrap().expect("must be rejected");
        assert_eq!(bad.problem, TrustProblem::WorldWritable);

        std::fs::remove_dir_all(&dir).unwrap();
    }
}

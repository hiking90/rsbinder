// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Name-service lookups (`getpwnam` / `getgrnam` / `getgrouplist`).
//!
//! Used to turn the human-readable `user = [...]` / `group = [...]` of a
//! policy file into the numeric ids the kernel actually reports, and to
//! answer "which groups does this uid belong to" for a caller.
//!
//! Everything here is the reentrant `_r` form: `rsb_hub` resolves groups
//! from more than one thread (the transaction thread, the death-notification
//! thread, and the client-callback poller), and the non-`_r` calls return a
//! pointer into a shared static buffer that a concurrent caller would
//! overwrite mid-use.

use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::sync::{Arc, Mutex};

/// Why a name-service lookup failed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NssError {
    /// The name is not in the user/group database.
    #[error("no such {kind}: {name:?}")]
    NotFound {
        /// `"user"` or `"group"`.
        kind: &'static str,
        /// The name that was looked up.
        name: String,
    },
    /// The name could not be passed to libc (embedded NUL).
    #[error("{kind} name contains an interior NUL: {name:?}")]
    InvalidName {
        /// `"user"` or `"group"`.
        kind: &'static str,
        /// The offending name.
        name: String,
    },
    /// libc reported an error.
    #[error("{call} failed for {name:?}: {message}")]
    Failed {
        /// The libc function that failed.
        call: &'static str,
        /// The name that was looked up.
        name: String,
        /// `strerror` of the returned errno.
        message: String,
    },
}

/// Upper bound on the scratch buffer the `_r` lookups grow into. A real
/// passwd/group entry is well under a kilobyte; a megabyte means the name
/// service is misbehaving and we should fail rather than allocate forever.
const MAX_NSS_BUF: usize = 1 << 20;

fn cstring(kind: &'static str, name: &str) -> Result<CString, NssError> {
    CString::new(name).map_err(|_| NssError::InvalidName {
        kind,
        name: name.to_owned(),
    })
}

/// Resolve a user name to its uid.
pub fn uid_for_user(name: &str) -> Result<u32, NssError> {
    let cname = cstring("user", name)?;
    let mut buf = vec![0u8; 1024];
    loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: `cname` is a valid NUL-terminated string alive across the
        // call; `pwd` and `found` are live locals; `buf` is a writable
        // allocation of exactly `buf.len()` bytes. `getpwnam_r` writes only
        // within those bounds and signals ERANGE instead of overrunning.
        let rc = unsafe {
            libc::getpwnam_r(
                cname.as_ptr(),
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_NSS_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(NssError::Failed {
                call: "getpwnam_r",
                name: name.to_owned(),
                message: std::io::Error::from_raw_os_error(rc).to_string(),
            });
        }
        if found.is_null() {
            return Err(NssError::NotFound {
                kind: "user",
                name: name.to_owned(),
            });
        }
        return Ok(pwd.pw_uid);
    }
}

/// Resolve a group name to its gid. A spec that is entirely digits is
/// taken as a literal gid and never touches the name service — policy has
/// to keep working in a minimal container with no group database.
pub fn gid_for_group(spec: &str) -> Result<u32, NssError> {
    if let Ok(gid) = spec.parse::<u32>() {
        return Ok(gid);
    }
    let cname = cstring("group", spec)?;
    let mut buf = vec![0u8; 1024];
    loop {
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as in `uid_for_user` — valid NUL-terminated name, live
        // out-params, and a buffer whose true length is passed alongside it.
        let rc = unsafe {
            libc::getgrnam_r(
                cname.as_ptr(),
                &mut grp,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_NSS_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 {
            return Err(NssError::Failed {
                call: "getgrnam_r",
                name: spec.to_owned(),
                message: std::io::Error::from_raw_os_error(rc).to_string(),
            });
        }
        if found.is_null() {
            return Err(NssError::NotFound {
                kind: "group",
                name: spec.to_owned(),
            });
        }
        return Ok(grp.gr_gid);
    }
}

/// `getgrouplist`'s group element type: `gid_t` on Linux, `int` on the
/// BSD-derived platforms (including macOS).
#[cfg(any(target_os = "linux", target_os = "android"))]
type GroupListId = libc::gid_t;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type GroupListId = libc::c_int;

/// One `getgrouplist` entry as a `gid_t`.
///
/// A no-op on Linux, where [`GroupListId`] already *is* `u32`, and a real
/// conversion on the BSD-derived platforms, where it is a signed `int`.
/// Written as a function so the `allow` covers exactly the cast that is
/// redundant on one target and required on the other.
#[allow(clippy::unnecessary_cast)]
fn gid_of(id: GroupListId) -> u32 {
    id as u32
}

/// Every group `uid` belongs to: its primary gid plus every supplementary
/// group. Returns an empty set if the uid is not in the user database —
/// an unknown uid simply matches no group rule, which under default-deny
/// is the safe outcome.
pub fn gids_for_uid(uid: u32) -> BTreeSet<u32> {
    let mut buf = vec![0u8; 1024];
    let (user, primary) = loop {
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: live out-params and a buffer whose true length is passed
        // alongside it; `getpwuid_r` reports ERANGE rather than overrunning.
        let rc = unsafe {
            libc::getpwuid_r(
                uid as libc::uid_t,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut found,
            )
        };
        if rc == libc::ERANGE && buf.len() < MAX_NSS_BUF {
            buf.resize(buf.len() * 2, 0);
            continue;
        }
        if rc != 0 || found.is_null() {
            return BTreeSet::new();
        }
        // SAFETY: on success `pw_name` points into `buf`, which outlives
        // this copy.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_owned();
        break (name, pwd.pw_gid);
    };

    let mut gids = BTreeSet::new();
    gids.insert(primary);

    let mut ngroups: libc::c_int = 32;
    loop {
        let mut groups: Vec<GroupListId> = vec![0; ngroups.max(1) as usize];
        // SAFETY: `user` is NUL-terminated and alive; `groups` has
        // `ngroups` writable elements and `ngroups` is passed by pointer so
        // libc can report the real count back. On overflow the call returns
        // -1 and leaves `ngroups` set to the needed size, which we retry.
        let rc = unsafe {
            libc::getgrouplist(
                user.as_ptr(),
                primary as GroupListId,
                groups.as_mut_ptr(),
                &mut ngroups,
            )
        };
        if rc < 0 {
            // Retry once at the size libc asked for. If it does not grow,
            // stop rather than spin.
            if ngroups as usize > groups.len() && (ngroups as usize) < 65536 {
                continue;
            }
            return gids;
        }
        gids.extend(
            groups
                .iter()
                .take(ngroups.max(0) as usize)
                .map(|g| gid_of(*g)),
        );
        return gids;
    }
}

/// Memoizes [`gids_for_uid`].
///
/// A name-service lookup per binder transaction would put an NSS round
/// trip (potentially LDAP or SSSD over a socket) on the critical path of
/// every `getService`. Group membership only changes administratively, so
/// the cache is held until [`clear`](Self::clear) — which `rsb_hub` calls
/// on the same SIGHUP that reloads the policy files.
#[derive(Default)]
pub struct GroupCache {
    entries: Mutex<HashMap<u32, Arc<BTreeSet<u32>>>>,
}

impl GroupCache {
    /// An empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Groups for `uid`, resolving and memoizing on first use.
    ///
    /// The NSS lookup runs *outside* the cache lock: it can block for as
    /// long as the name service takes, and holding the lock across it would
    /// serialize every other thread's lookups behind the slowest one. A
    /// concurrent duplicate resolve is possible and harmless — both
    /// produce the same set and the first insert wins.
    pub fn gids_for(&self, uid: u32) -> Arc<BTreeSet<u32>> {
        if let Some(hit) = self
            .entries
            .lock()
            .expect("group cache poisoned")
            .get(&uid)
            .cloned()
        {
            return hit;
        }
        let resolved = Arc::new(gids_for_uid(uid));
        self.entries
            .lock()
            .expect("group cache poisoned")
            .entry(uid)
            .or_insert_with(|| Arc::clone(&resolved))
            .clone()
    }

    /// Drop every memoized entry, so the next lookup re-reads the name
    /// service. Called on policy reload.
    pub fn clear(&self) {
        self.entries.lock().expect("group cache poisoned").clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A numeric spec must bypass NSS entirely.
    #[test]
    fn numeric_group_spec_bypasses_nss() {
        assert_eq!(gid_for_group("0").unwrap(), 0);
        assert_eq!(gid_for_group("1234").unwrap(), 1234);
    }

    #[test]
    fn unknown_names_report_not_found() {
        assert_eq!(
            gid_for_group("rsbinder-no-such-group-ffffffff"),
            Err(NssError::NotFound {
                kind: "group",
                name: "rsbinder-no-such-group-ffffffff".to_owned()
            })
        );
        assert_eq!(
            uid_for_user("rsbinder-no-such-user-ffffffff"),
            Err(NssError::NotFound {
                kind: "user",
                name: "rsbinder-no-such-user-ffffffff".to_owned()
            })
        );
    }

    #[test]
    fn interior_nul_is_rejected_not_truncated() {
        assert!(matches!(
            uid_for_user("ro\0ot"),
            Err(NssError::InvalidName { kind: "user", .. })
        ));
    }

    /// `root` exists on every platform this builds for, and uid 0's group
    /// set must at least contain its own primary group.
    #[test]
    fn root_resolves_and_has_groups() {
        let uid = uid_for_user("root").expect("root must exist");
        assert_eq!(uid, 0);
        assert!(!gids_for_uid(0).is_empty());
    }

    /// An unknown uid yields an empty set rather than panicking — under
    /// default-deny that simply matches no group rule.
    #[test]
    fn unknown_uid_yields_empty_group_set() {
        assert!(gids_for_uid(4_000_000_000).is_empty());
    }

    #[test]
    fn cache_returns_the_same_set_and_clears() {
        let cache = GroupCache::new();
        let first = cache.gids_for(0);
        let second = cache.gids_for(0);
        assert!(
            Arc::ptr_eq(&first, &second),
            "second call must hit the cache"
        );
        cache.clear();
        assert_eq!(*cache.gids_for(0), *first, "content survives a clear");
    }
}

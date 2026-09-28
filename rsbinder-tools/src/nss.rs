// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Name-service lookups (`getpwnam` / `getgrnam` / `getgrouplist`).
//!
//! Used to turn the human-readable `user = [...]` / `group = [...]` of a
//! policy file into the numeric ids the kernel actually reports, and to
//! answer "which groups does this uid belong to" for a caller.
//!
//! These run on more than one thread — `rsb_hub` resolves callers' groups
//! on its one binder thread while a SIGHUP reload resolves policy names on
//! its signal thread, and a hub built with the `rpc` feature serves from
//! several binder threads — so every lookup here must be reentrant. On
//! glibc and the BSDs that means the `_r` forms, because the plain ones
//! return a pointer into a shared static buffer a concurrent caller would
//! overwrite mid-use. Android is the exception, and [`gid_for_group`]
//! explains why.

use std::collections::{BTreeSet, HashMap};
use std::ffi::CString;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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

/// Cap on the `_r` scratch buffer: a real entry is under 1 KiB, so 1 MiB means a broken NSS.
const MAX_NSS_BUF: usize = 1 << 20;

/// One `_r` lookup's outcome, decided in one place for every libc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Lookup {
    Found,
    NotFound,
    /// The lookup could not answer; the errno it returned.
    Failed(libc::c_int),
}

/// glibc/BSD: 0 = answered, else a module's errno (`nss/getXXbyYY_r.c`); bionic: ENOENT = absent.
pub(crate) fn classify(rc: libc::c_int, found: bool) -> Lookup {
    match (rc, found) {
        (0, true) => Lookup::Found,
        (0, false) => Lookup::NotFound,
        (rc, _) if cfg!(target_os = "android") && rc == libc::ENOENT => Lookup::NotFound,
        (rc, _) => Lookup::Failed(rc),
    }
}

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
        // SAFETY: `passwd` holds only integers and raw pointers; all-zero is a valid value.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: NUL-terminated `cname`, live out-params, true `buf.len()`; ERANGE, not overrun.
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
        return match classify(rc, !found.is_null()) {
            Lookup::Found => Ok(pwd.pw_uid),
            Lookup::NotFound => Err(NssError::NotFound {
                kind: "user",
                name: name.to_owned(),
            }),
            Lookup::Failed(rc) => Err(NssError::Failed {
                call: "getpwnam_r",
                name: name.to_owned(),
                message: std::io::Error::from_raw_os_error(rc).to_string(),
            }),
        };
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
    lookup_gid(&cname, spec)
}

/// `getgrnam_r`, growing the buffer until libc stops asking for more.
#[cfg(not(target_os = "android"))]
fn lookup_gid(cname: &std::ffi::CStr, spec: &str) -> Result<u32, NssError> {
    let mut buf = vec![0u8; 1024];
    loop {
        // SAFETY: `group` holds only integers and raw pointers; all-zero is a valid value.
        let mut grp: libc::group = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::group = std::ptr::null_mut();
        // SAFETY: as in `uid_for_user`: NUL-terminated name, live out-params, true buffer length.
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
        return match classify(rc, !found.is_null()) {
            Lookup::Found => Ok(grp.gr_gid),
            Lookup::NotFound => Err(NssError::NotFound {
                kind: "group",
                name: spec.to_owned(),
            }),
            Lookup::Failed(rc) => Err(NssError::Failed {
                call: "getgrnam_r",
                name: spec.to_owned(),
                message: std::io::Error::from_raw_os_error(rc).to_string(),
            }),
        };
    }
}

/// `getgrnam_r` fails to link below API 24; bionic's `getgrnam` is reentrant (per-thread buffer).
#[cfg(target_os = "android")]
fn lookup_gid(cname: &std::ffi::CStr, spec: &str) -> Result<u32, NssError> {
    // Null is not-found (bionic sets ENOENT) or failure; clear errno so a stale value is not read.
    // SAFETY: `__errno()` returns this thread's errno slot, valid to write.
    unsafe { *libc::__errno() = 0 };
    // SAFETY: `cname` is live and NUL-terminated; result is this thread's buffer, never stored.
    let grp = unsafe { libc::getgrnam(cname.as_ptr()) };
    // errno means something only on a miss; 0 there (nothing set) classifies as not-found.
    let rc = if grp.is_null() {
        std::io::Error::last_os_error().raw_os_error().unwrap_or(0)
    } else {
        0
    };
    match classify(rc, !grp.is_null()) {
        // SAFETY: non-null, points into this thread's bionic group buffer, untouched since.
        Lookup::Found => Ok(unsafe { (*grp).gr_gid }),
        Lookup::NotFound => Err(NssError::NotFound {
            kind: "group",
            name: spec.to_owned(),
        }),
        Lookup::Failed(rc) => Err(NssError::Failed {
            call: "getgrnam",
            name: spec.to_owned(),
            message: std::io::Error::from_raw_os_error(rc).to_string(),
        }),
    }
}

/// `getgrouplist`'s element type: `gid_t` on Linux, `int` on the BSD-derived platforms.
#[cfg(any(target_os = "linux", target_os = "android"))]
type GroupListId = libc::gid_t;
#[cfg(not(any(target_os = "linux", target_os = "android")))]
type GroupListId = libc::c_int;

/// Whether a short `getgrouplist` buffer leaves the needed size in `*ngroups` (glibc, bionic).
const GETGROUPLIST_REPORTS_NEED: bool = cfg!(any(target_os = "linux", target_os = "android"));

/// The cast is redundant on Linux (`u32`) but not on BSDs (`int`); a fn scopes the `allow` to it.
#[allow(clippy::unnecessary_cast)]
fn gid_of(id: GroupListId) -> u32 {
    id as u32
}

/// The groups `uid` belongs to: its primary gid plus the supplementary
/// groups the name service returned. An empty set means the uid is not in
/// the user database — an unknown uid simply matches no group rule, which
/// under default-deny is the safe outcome.
///
/// `None` means the passwd lookup failed (an NSS error such as an
/// unreachable LDAP/SSSD backend), or the group list could not be read at
/// all.
///
/// **`Some` can be a subset of the real membership.** glibc's
/// `getgrouplist` returns what the backends it reached produced and does not
/// report one that was `UNAVAIL` (`nss/initgroups.c`
/// `internal_getgrouplist`), so with `group: files sss` and sssd down the
/// answer is the `files` groups alone, as a success. The caller cannot tell
/// such an answer from a complete one; [`GroupCache`] handles it by
/// re-reading on a deny instead.
pub fn gids_for_uid(uid: u32) -> Option<BTreeSet<u32>> {
    let mut buf = vec![0u8; 1024];
    let (user, primary) = loop {
        // SAFETY: `passwd` holds only integers and raw pointers; all-zero is a valid value.
        let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
        let mut found: *mut libc::passwd = std::ptr::null_mut();
        // SAFETY: live out-params, true `buf.len()`; `getpwuid_r` reports ERANGE, not overrun.
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
        match classify(rc, !found.is_null()) {
            Lookup::Found => {}
            Lookup::NotFound => return Some(BTreeSet::new()),
            Lookup::Failed(_) => return None,
        }
        // SAFETY: on success `pw_name` points into `buf`, which outlives this copy.
        let name = unsafe { std::ffi::CStr::from_ptr(pwd.pw_name) }.to_owned();
        break (name, pwd.pw_gid);
    };

    let mut gids = BTreeSet::new();
    gids.insert(primary);

    let mut ngroups: libc::c_int = 32;
    loop {
        let mut groups: Vec<GroupListId> = vec![0; ngroups.max(1) as usize];
        // SAFETY: `user` is live and NUL-terminated; `groups` has `ngroups` writable elements.
        let rc = unsafe {
            libc::getgrouplist(
                user.as_ptr(),
                primary as GroupListId,
                groups.as_mut_ptr(),
                &mut ngroups,
            )
        };
        if rc < 0 {
            let filled = ngroups as usize <= groups.len();
            // BSD-derived libcs (suspected: macOS Libinfo) report the count filled, not the need.
            let need = if filled && !GETGROUPLIST_REPORTS_NEED {
                groups.len() * 2
            } else {
                ngroups as usize
            };
            // Retry only while the buffer grows; otherwise stop rather than spin.
            if need > groups.len() && need < 65536 {
                ngroups = need as libc::c_int;
                continue;
            }
            if GETGROUPLIST_REPORTS_NEED {
                // Nothing was read (glibc malloc failure); the primary alone would look complete.
                return None;
            }
        }
        gids.extend(
            groups
                .iter()
                .take(ngroups.max(0) as usize)
                .map(|g| gid_of(*g)),
        );
        return Some(gids);
    }
}

/// What [`GroupCache`] calls to read a uid's groups; [`gids_for_uid`] unless a test injects one.
pub type Resolver = dyn Fn(u32) -> Option<BTreeSet<u32>> + Send + Sync;

struct Entry {
    gids: Arc<BTreeSet<u32>>,
    /// When the resolver was last asked for this uid, whether or not it answered.
    resolved_at: Instant,
}

/// Memoizes [`gids_for_uid`], re-reading an entry when it could turn a deny
/// into an allow.
///
/// A name-service lookup per binder transaction would put an NSS round
/// trip (potentially LDAP or SSSD over a socket) on the critical path of
/// every `getService`. So a uid's groups are read once and held until
/// [`clear`](Self::clear) — which `rsb_hub` calls on the same SIGHUP that
/// reloads the policy files — with one exception: `Enforcer` calls
/// [`revalidate`](Self::revalidate) when a group rule denied the caller,
/// which re-reads the entry at most once per
/// [`MIN_REVALIDATE`](Self::MIN_REVALIDATE).
///
/// The exception exists because an entry can be smaller than the real
/// membership: [`gids_for_uid`] may return a subset while a backend is
/// unreachable, and a failed first lookup is held as an empty set. A group
/// *added* to the uid is picked up by the same re-read. A re-read only adds
/// to the entry, since it cannot tell a complete answer from a subset, so a
/// group *removed* from the uid keeps granting until the SIGHUP.
///
/// **Correctness depends on the policy being allow-only** (`Subjects` has
/// no negative form, and `Policy::check` picks the governing rule by name
/// alone): a cached set that is too small can only produce a wrong deny,
/// and a deny is what triggers the re-read. A "not in group" predicate in
/// the policy would break this.
pub struct GroupCache {
    entries: Mutex<HashMap<u32, Entry>>,
    /// Bumped by `clear` under the `entries` lock, so a resolve straddling a clear is not cached.
    generation: AtomicU64,
    resolver: Box<Resolver>,
}

impl Default for GroupCache {
    fn default() -> Self {
        Self::with_resolver(Box::new(gids_for_uid))
    }
}

impl GroupCache {
    /// The shortest interval between two name-service reads for one uid.
    ///
    /// It bounds the NSS calls a denied caller can cause; it does not
    /// decide any verdict. 15 s is sssd's `entry_negative_timeout` default
    /// (`sssd.conf(5)`), the shortest negative TTL among the common
    /// backends, so asking sooner gets sssd's cached negative answer back.
    /// nscd's defaults are longer (`nscd.conf` `negative-time-to-live`:
    /// passwd 20 s, group 60 s). A longer interval only delays noticing a
    /// recovered backend.
    pub const MIN_REVALIDATE: Duration = Duration::from_secs(15);

    /// An empty cache over the system name service ([`gids_for_uid`]).
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty cache that reads groups through `resolver` instead of the
    /// system name service.
    pub fn with_resolver(resolver: Box<Resolver>) -> Self {
        GroupCache {
            entries: Mutex::new(HashMap::new()),
            generation: AtomicU64::new(0),
            resolver,
        }
    }

    /// Groups for `uid`, resolving and memoizing on first use.
    ///
    /// The NSS lookup runs *outside* the cache lock: it can block for as
    /// long as the name service takes, and holding the lock across it would
    /// serialize every other thread's lookups behind the slowest one. A
    /// concurrent duplicate resolve is possible; the first insert wins.
    ///
    /// A failed lookup is memoized as an empty set, so a name-service
    /// outage costs one lookup per [`MIN_REVALIDATE`](Self::MIN_REVALIDATE)
    /// per uid rather than one per transaction; the entry is revalidated on
    /// the next deny. A result is not memoized when [`clear`](Self::clear)
    /// ran while the lookup was in flight: that lookup may have read group
    /// membership from before the reload.
    pub fn gids_for(&self, uid: u32) -> Arc<BTreeSet<u32>> {
        let generation = {
            let entries = self.entries.lock().expect("group cache poisoned");
            if let Some(hit) = entries.get(&uid) {
                return Arc::clone(&hit.gids);
            }
            self.generation.load(Ordering::Relaxed)
        };
        let resolved = Arc::new((self.resolver)(uid).unwrap_or_default());
        let mut entries = self.entries.lock().expect("group cache poisoned");
        if self.generation.load(Ordering::Relaxed) != generation {
            return resolved;
        }
        let entry = entries.entry(uid).or_insert_with(|| Entry {
            gids: resolved,
            resolved_at: Instant::now(),
        });
        Arc::clone(&entry.gids)
    }

    /// Re-read `uid`'s groups unless that was done within
    /// [`MIN_REVALIDATE`](Self::MIN_REVALIDATE), and return the set now in
    /// force. For a caller a group rule denied: the cached set may be
    /// smaller than the real one (see the type docs).
    ///
    /// Within `MIN_REVALIDATE` of the previous read, successful or not, the
    /// cached set is returned without a lookup. Otherwise the entry's
    /// timestamp is advanced under the lock *before* the lookup, so
    /// concurrent callers for the same uid do not all query the name
    /// service; the lookup itself runs outside the lock. A successful read
    /// is merged into the entry, which never shrinks: a read made while a
    /// backend is down can be a subset (see [`gids_for_uid`]), and replacing
    /// a complete set with it would deny callers the policy allows. A
    /// failed read keeps the entry as it is. A read
    /// that straddled [`clear`](Self::clear) is returned but not stored, as
    /// in [`gids_for`](Self::gids_for). A uid with no entry is resolved by
    /// `gids_for`.
    pub fn revalidate(&self, uid: u32) -> Arc<BTreeSet<u32>> {
        let (prev, generation) = {
            let mut entries = self.entries.lock().expect("group cache poisoned");
            let Some(entry) = entries.get_mut(&uid) else {
                drop(entries);
                return self.gids_for(uid);
            };
            if entry.resolved_at.elapsed() < Self::MIN_REVALIDATE {
                return Arc::clone(&entry.gids);
            }
            entry.resolved_at = Instant::now();
            (
                Arc::clone(&entry.gids),
                self.generation.load(Ordering::Relaxed),
            )
        };
        let Some(resolved) = (self.resolver)(uid) else {
            return prev;
        };
        let mut entries = self.entries.lock().expect("group cache poisoned");
        if self.generation.load(Ordering::Relaxed) != generation {
            return Arc::new(resolved);
        }
        let entry = entries.entry(uid).or_insert_with(|| Entry {
            gids: Arc::default(),
            resolved_at: Instant::now(),
        });
        if !resolved.is_subset(&entry.gids) {
            entry.gids = Arc::new(entry.gids.union(&resolved).copied().collect());
        }
        Arc::clone(&entry.gids)
    }

    /// Drop every memoized entry, so the next lookup re-reads the name
    /// service. Called on policy reload.
    pub fn clear(&self) {
        let mut entries = self.entries.lock().expect("group cache poisoned");
        self.generation.fetch_add(1, Ordering::Relaxed);
        entries.clear();
    }

    /// Move every entry's last read `by` into the past.
    #[cfg(test)]
    pub(crate) fn age_entries(&self, by: Duration) {
        let mut entries = self.entries.lock().expect("group cache poisoned");
        for entry in entries.values_mut() {
            entry.resolved_at = entry.resolved_at.checked_sub(by).expect("clock too young");
        }
    }

    /// A cache answering `responses` in order (the last one repeats), plus its resolver call count.
    #[cfg(test)]
    pub(crate) fn scripted(
        responses: Vec<Option<BTreeSet<u32>>>,
    ) -> (Self, Arc<std::sync::atomic::AtomicUsize>) {
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let queue = Mutex::new(std::collections::VecDeque::from(responses));
        let counter = Arc::clone(&calls);
        let resolver = move |_uid| {
            counter.fetch_add(1, Ordering::Relaxed);
            let mut queue = queue.lock().unwrap();
            match queue.len() {
                0 | 1 => queue.front().cloned().flatten(),
                _ => queue.pop_front().flatten(),
            }
        };
        (Self::with_resolver(Box::new(resolver)), calls)
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

    #[test]
    fn root_resolves_and_has_groups() {
        let uid = uid_for_user("root").expect("root must exist");
        assert_eq!(uid, 0);
        let gids = gids_for_uid(0).expect("uid 0 lookup must not fail");
        assert!(!gids.is_empty());
    }

    /// Not-found is an answer (`Some`, cacheable), not a lookup failure (`None`).
    #[test]
    fn unknown_uid_yields_empty_group_set() {
        assert_eq!(gids_for_uid(4_000_000_000), Some(BTreeSet::new()));
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

    fn set(gids: &[u32]) -> BTreeSet<u32> {
        gids.iter().copied().collect()
    }

    fn calls(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(Ordering::Relaxed)
    }

    /// glibc's not-found is `rc == 0`; an errno is a failed module, whatever its value.
    #[test]
    fn classify_maps_rc_and_found() {
        assert_eq!(classify(0, true), Lookup::Found);
        assert_eq!(classify(0, false), Lookup::NotFound);
        #[cfg(not(target_os = "android"))]
        for rc in [
            libc::ENOENT,
            libc::ESRCH,
            libc::EBADF,
            libc::EPERM,
            libc::EIO,
        ] {
            assert_eq!(classify(rc, false), Lookup::Failed(rc), "rc {rc}");
        }
        #[cfg(target_os = "android")]
        {
            assert_eq!(classify(libc::ENOENT, false), Lookup::NotFound);
            assert_eq!(classify(libc::EIO, false), Lookup::Failed(libc::EIO));
        }
    }

    /// A stream of re-reads for one uid costs one lookup per `MIN_REVALIDATE`.
    #[test]
    fn revalidation_is_throttled_per_uid() {
        let (cache, counter) = GroupCache::scripted(vec![Some(set(&[]))]);
        cache.gids_for(1);
        for _ in 0..100 {
            cache.revalidate(1);
        }
        assert_eq!(calls(&counter), 1);
        cache.age_entries(GroupCache::MIN_REVALIDATE);
        cache.revalidate(1);
        assert_eq!(calls(&counter), 2);
        for _ in 0..100 {
            cache.revalidate(1);
        }
        assert_eq!(calls(&counter), 2);
    }

    /// A cache whose resolver calls `clear` on that same cache on the given call (1-based).
    fn clearing_on_call(clear_on: usize, answer: BTreeSet<u32>) -> Arc<GroupCache> {
        let slot: Arc<std::sync::OnceLock<std::sync::Weak<GroupCache>>> = Arc::default();
        let seen = std::sync::atomic::AtomicUsize::new(0);
        let resolver = {
            let slot = Arc::clone(&slot);
            move |_uid| {
                if seen.fetch_add(1, Ordering::Relaxed) + 1 == clear_on {
                    if let Some(cache) = slot.get().and_then(std::sync::Weak::upgrade) {
                        cache.clear();
                    }
                }
                Some(answer.clone())
            }
        };
        let cache = Arc::new(GroupCache::with_resolver(Box::new(resolver)));
        slot.set(Arc::downgrade(&cache)).unwrap();
        cache
    }

    /// A first read that straddles a reload is answered but not stored.
    #[test]
    fn clear_during_resolve_is_not_cached() {
        let cache = clearing_on_call(1, set(&[50]));
        assert_eq!(*cache.gids_for(1), set(&[50]));
        assert!(!cache.entries.lock().unwrap().contains_key(&1));
    }

    /// A re-read that straddles a reload is answered but not stored.
    #[test]
    fn clear_during_revalidate_is_not_cached() {
        let cache = clearing_on_call(2, set(&[50]));
        cache.gids_for(1);
        cache.age_entries(GroupCache::MIN_REVALIDATE);
        assert_eq!(*cache.revalidate(1), set(&[50]));
        assert!(cache.entries.lock().unwrap().is_empty());
    }

    /// A failed re-read keeps what was known and still counts against the throttle.
    #[test]
    fn failed_revalidation_keeps_entry_and_throttles() {
        let (cache, counter) =
            GroupCache::scripted(vec![Some(set(&[50])), None, Some(set(&[50, 60]))]);
        assert_eq!(*cache.gids_for(1), set(&[50]));
        cache.age_entries(GroupCache::MIN_REVALIDATE);
        assert_eq!(*cache.revalidate(1), set(&[50]));
        assert_eq!(calls(&counter), 2);
        assert_eq!(*cache.revalidate(1), set(&[50]));
        assert_eq!(calls(&counter), 2);
        cache.age_entries(GroupCache::MIN_REVALIDATE);
        assert_eq!(*cache.revalidate(1), set(&[50, 60]));
        assert_eq!(calls(&counter), 3);
    }

    /// An outage costs one lookup per `MIN_REVALIDATE` per uid, not one per call.
    #[test]
    fn failed_first_lookup_is_cached_as_empty_and_throttled() {
        let (cache, counter) = GroupCache::scripted(vec![None]);
        for _ in 0..10 {
            assert!(cache.gids_for(1).is_empty());
        }
        assert_eq!(calls(&counter), 1);
        for _ in 0..10 {
            assert!(cache.revalidate(1).is_empty());
        }
        assert_eq!(calls(&counter), 1);
        cache.age_entries(GroupCache::MIN_REVALIDATE);
        cache.revalidate(1);
        assert_eq!(calls(&counter), 2);
    }
}

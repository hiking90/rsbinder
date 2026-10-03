// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Runtime side of the configuration: holds the loaded [`Config`], memoizes
//! group lookups, and answers the two questions the hub asks of it — may
//! this caller do this, and what is declared?
//!
//! One holder for both halves so a reload swaps them together. Split across
//! two fields they could drift: a SIGHUP that updated the rules but not the
//! declarations would enforce one file against another.

use std::sync::{Arc, RwLock};

use rsbinder::Caller;

use super::parse::Config;
use super::policy::{Permission, Subject};
use crate::nss::GroupCache;

/// Applies a [`Policy`](super::policy::Policy) to live callers.
///
/// Held behind a shared reference for the lifetime of `rsb_hub`; the
/// policy inside can be swapped by [`replace`](Self::replace)
/// on SIGHUP without disturbing in-flight transactions.
pub struct Enforcer {
    config: RwLock<Arc<Config>>,
    /// Set by `--insecure-allow-all`. Short-circuits every check.
    allow_all: bool,
    groups: GroupCache,
}

impl Enforcer {
    /// An enforcer that applies `config`.
    pub fn enforcing(config: Config) -> Self {
        Enforcer {
            config: RwLock::new(Arc::new(config)),
            allow_all: false,
            groups: GroupCache::new(),
        }
    }

    /// An enforcer over a bare policy, for tests that do not care about declarations.
    #[cfg(test)]
    fn from_policy(policy: super::policy::Policy) -> Self {
        Enforcer::enforcing(Config {
            policy,
            ..Config::default()
        })
    }

    /// An enforcer over a bare policy whose group lookups go through `groups`.
    #[cfg(test)]
    fn from_policy_with(policy: super::policy::Policy, groups: GroupCache) -> Self {
        Enforcer {
            config: RwLock::new(Arc::new(Config {
                policy,
                ..Config::default()
            })),
            allow_all: false,
            groups,
        }
    }

    /// An enforcer that permits everything — `--insecure-allow-all`.
    ///
    /// Only for development and for the integration suite, which spins up
    /// throwaway services under names no policy file could know in advance.
    /// It is a separate constructor rather than an empty policy so that
    /// "allow everything" can never be the result of a policy that merely
    /// failed to load.
    /// Nothing is declared in this mode: `--insecure-allow-all` means there
    /// is no configuration file, so there is nothing to have declared it.
    pub fn allow_all() -> Self {
        Enforcer {
            config: RwLock::new(Arc::new(Config::default())),
            allow_all: true,
            groups: GroupCache::new(),
        }
    }

    /// Is this enforcer in `--insecure-allow-all` mode?
    pub fn is_allow_all(&self) -> bool {
        self.allow_all
    }

    /// Swap in a freshly loaded configuration and drop the memoized group
    /// sets, so a reload also picks up group-membership changes.
    ///
    /// Only checks made after the swap see the new configuration. A grant
    /// already acted on is not revisited: `rsb_hub` checks `find` when a
    /// `registerForNotifications` callback is registered, not each time it
    /// fires, as AOSP `ServiceManager::dispatchRegistrationCallbacks` does.
    pub fn replace(&self, config: Config) {
        *self.config.write().expect("config lock poisoned") = Arc::new(config);
        self.groups.clear();
    }

    /// The configuration currently in force. Cloned out of the lock so the
    /// caller never holds it across a binder call.
    pub fn config(&self) -> Arc<Config> {
        Arc::clone(&self.config.read().expect("config lock poisoned"))
    }

    /// Resolve `uid` into the subject the evaluator matches against.
    ///
    /// This is the memoized view; [`check_uid`](Self::check_uid) is the
    /// entry point that re-reads the groups on a deny.
    pub fn subject_for(&self, uid: u32) -> Subject {
        Subject {
            uid,
            gids: (*self.groups.gids_for(uid)).clone(),
        }
    }

    /// May `uid` exercise `permission` on `name`?
    ///
    /// A deny that a group rule could have turned into an allow is judged
    /// again after [`GroupCache::revalidate`], since the memoized groups
    /// may be incomplete (see [`GroupCache`]); that re-read happens at most
    /// once per [`GroupCache::MIN_REVALIDATE`] per uid.
    pub fn check_uid(&self, permission: Permission, name: &str, uid: u32) -> bool {
        if self.allow_all {
            return true;
        }
        let config = self.config();
        let policy = &config.policy;
        // Judged against the memoized set in place; `subject_for` would copy it.
        if policy.check_parts(permission, name, uid, &self.groups.gids_for(uid)) {
            return true;
        }
        if !policy.group_could_grant(permission, name) {
            return false;
        }
        policy.check_parts(permission, name, uid, &self.groups.revalidate(uid))
    }

    /// May `caller` exercise `permission` on `name`?
    ///
    /// Identities the policy cannot key on are denied: this model is
    /// uid-based, and a vsock cid is a routing address rather than a
    /// principal, a TLS certificate needs a certificate→principal mapping
    /// that does not exist here, and an anonymous peer has no identity at
    /// all. Denying is the only honest answer for those — see
    /// `rsbinder::rpc::PeerIdentity`.
    pub fn check_caller(&self, permission: Permission, name: &str, caller: &Caller) -> bool {
        match uid_of(caller) {
            Some(uid) => self.check_uid(permission, name, uid),
            None => false,
        }
    }
}

/// The uid a policy can be keyed on, if this caller has one.
fn uid_of(caller: &Caller) -> Option<u32> {
    match caller {
        Caller::Kernel { uid, .. } => Some(*uid),
        #[cfg(feature = "rpc")]
        Caller::Rpc(rsbinder::rpc::PeerIdentity::Local { uid, .. }) => Some(*uid),
        // No uid to key on; `Caller` is `#[non_exhaustive]`, so a new transport lands here too.
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{NamePattern, Policy, Rule, Subjects};

    fn policy_allowing_uid(uid: u32) -> Policy {
        Policy {
            list: Subjects::set([uid], []),
            rules: vec![Rule {
                pattern: NamePattern::Any,
                add: Subjects::set([uid], []),
                find: Subjects::set([uid], []),
            }],
        }
    }

    #[test]
    fn enforcing_follows_the_policy() {
        let enforcer = Enforcer::from_policy(policy_allowing_uid(1000));
        assert!(enforcer.check_uid(Permission::Add, "svc", 1000));
        assert!(!enforcer.check_uid(Permission::Add, "svc", 1001));
        assert!(!enforcer.is_allow_all());
    }

    #[test]
    fn allow_all_short_circuits() {
        let enforcer = Enforcer::allow_all();
        assert!(enforcer.is_allow_all());
        for perm in [Permission::Add, Permission::Find, Permission::List] {
            assert!(enforcer.check_uid(perm, "anything", 4_000_000_000));
        }
    }

    #[test]
    fn replace_policy_takes_effect() {
        let enforcer = Enforcer::from_policy(policy_allowing_uid(1000));
        assert!(enforcer.check_uid(Permission::Find, "svc", 1000));
        enforcer.replace(Config {
            policy: policy_allowing_uid(2000),
            ..Config::default()
        });
        assert!(!enforcer.check_uid(Permission::Find, "svc", 1000));
        assert!(enforcer.check_uid(Permission::Find, "svc", 2000));
    }

    /// A reload must not leave a permissive window: a deny-all swap denies at once.
    #[test]
    fn replace_with_deny_all_denies() {
        let enforcer = Enforcer::from_policy(policy_allowing_uid(1000));
        enforcer.replace(Config::default());
        assert!(!enforcer.check_uid(Permission::Add, "svc", 1000));
    }

    #[test]
    fn kernel_caller_is_keyed_on_uid() {
        let enforcer = Enforcer::from_policy(policy_allowing_uid(1000));
        let caller = Caller::Kernel {
            uid: 1000,
            pid: 4321,
            sid: None,
        };
        assert!(enforcer.check_caller(Permission::Add, "svc", &caller));
    }

    /// A uid policy cannot judge an identity with no uid, so it is refused, not defaulted.
    #[cfg(feature = "rpc")]
    #[test]
    fn identities_without_a_uid_are_denied() {
        let enforcer = Enforcer::from_policy(policy_allowing_uid(1000));
        for peer in [
            rsbinder::rpc::PeerIdentity::Anonymous,
            rsbinder::rpc::PeerIdentity::Vsock { cid: 3 },
        ] {
            assert!(!enforcer.check_caller(Permission::Find, "svc", &Caller::Rpc(peer)));
        }
        assert!(enforcer.check_caller(
            Permission::Find,
            "svc",
            &Caller::Rpc(rsbinder::rpc::PeerIdentity::Local {
                uid: 1000,
                pid: 4321
            })
        ));
    }

    fn policy_allowing_group(gid: u32) -> Policy {
        Policy {
            list: Subjects::set([], [gid]),
            rules: vec![Rule {
                pattern: NamePattern::Any,
                add: Subjects::None,
                find: Subjects::set([], [gid]),
            }],
        }
    }

    fn gids(gids: &[u32]) -> Option<std::collections::BTreeSet<u32>> {
        Some(gids.iter().copied().collect())
    }

    fn calls(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A lookup that failed on first use must not deny the caller until SIGHUP.
    #[test]
    fn failed_first_lookup_is_revalidated_on_deny() {
        let (groups, counter) = GroupCache::scripted(vec![None, gids(&[50])]);
        let enforcer = Enforcer::from_policy_with(policy_allowing_group(50), groups);
        assert!(!enforcer.check_uid(Permission::Find, "svc", 7));
        assert_eq!(calls(&counter), 1);
        enforcer.groups.age_entries(GroupCache::MIN_REVALIDATE);
        assert!(enforcer.check_uid(Permission::Find, "svc", 7));
        assert_eq!(calls(&counter), 2);
    }

    /// glibc returns a subset as a success when one backend is down; a deny re-reads it.
    #[test]
    fn partial_set_is_revalidated_on_deny() {
        let (groups, counter) = GroupCache::scripted(vec![gids(&[100]), gids(&[100, 50])]);
        let enforcer = Enforcer::from_policy_with(policy_allowing_group(50), groups);
        assert!(!enforcer.check_uid(Permission::Find, "svc", 7));
        enforcer.groups.age_entries(GroupCache::MIN_REVALIDATE);
        assert!(enforcer.check_uid(Permission::Find, "svc", 7));
        assert_eq!(calls(&counter), 2);
    }

    /// No group can change a uid-only verdict, so its deny costs no lookup.
    #[test]
    fn uid_only_rule_never_revalidates() {
        let (groups, counter) = GroupCache::scripted(vec![gids(&[])]);
        let enforcer = Enforcer::from_policy_with(policy_allowing_uid(1), groups);
        assert!(!enforcer.check_uid(Permission::Find, "svc", 2));
        enforcer.groups.age_entries(GroupCache::MIN_REVALIDATE);
        for _ in 0..3 {
            assert!(!enforcer.check_uid(Permission::Find, "svc", 2));
        }
        assert_eq!(calls(&counter), 1);
    }

    /// An allow never re-reads, however old the entry.
    #[test]
    fn allow_path_never_touches_the_resolver_again() {
        let (groups, counter) = GroupCache::scripted(vec![gids(&[50])]);
        let enforcer = Enforcer::from_policy_with(policy_allowing_group(50), groups);
        assert!(enforcer.check_uid(Permission::Find, "svc", 7));
        enforcer.groups.age_entries(GroupCache::MIN_REVALIDATE);
        for _ in 0..10 {
            assert!(enforcer.check_uid(Permission::Find, "svc", 7));
        }
        assert_eq!(calls(&counter), 1);
    }

    /// A re-read during an outage returns a subset; it must not take away a group already seen.
    #[test]
    fn a_subset_reread_does_not_revoke_a_cached_group() {
        let policy = Policy {
            list: Subjects::None,
            rules: vec![
                Rule {
                    pattern: NamePattern::Exact("only70".into()),
                    add: Subjects::None,
                    find: Subjects::set([], [70]),
                },
                Rule {
                    pattern: NamePattern::Any,
                    add: Subjects::None,
                    find: Subjects::set([], [50]),
                },
            ],
        };
        let (groups, counter) = GroupCache::scripted(vec![gids(&[100, 50]), gids(&[100])]);
        let enforcer = Enforcer::from_policy_with(policy, groups);
        assert!(enforcer.check_uid(Permission::Find, "svc", 7));
        enforcer.groups.age_entries(GroupCache::MIN_REVALIDATE);
        assert!(!enforcer.check_uid(Permission::Find, "only70", 7));
        assert_eq!(calls(&counter), 2);
        assert!(enforcer.check_uid(Permission::Find, "svc", 7));
    }
}

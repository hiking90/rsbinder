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

    /// An enforcer over a bare policy, for tests that do not care about
    /// declarations.
    #[cfg(test)]
    fn from_policy(policy: super::policy::Policy) -> Self {
        Enforcer::enforcing(Config {
            policy,
            ..Config::default()
        })
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
    pub fn subject_for(&self, uid: u32) -> Subject {
        Subject {
            uid,
            gids: (*self.groups.gids_for(uid)).clone(),
        }
    }

    /// May `uid` exercise `permission` on `name`?
    pub fn check_uid(&self, permission: Permission, name: &str, uid: u32) -> bool {
        if self.allow_all {
            return true;
        }
        let config = self.config();
        config
            .policy
            .check(permission, name, &self.subject_for(uid))
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
        // Everything else has no uid to key on — a vsock cid is a routing
        // address, a TLS certificate needs a mapping that does not exist
        // here, an anonymous peer has no identity at all, and `Caller` is
        // `#[non_exhaustive]` so a transport added later lands here too
        // until this match is taught about it.
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

    /// A reload must not leave a permissive window: swapping in a
    /// deny-all policy denies immediately.
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

    /// An identity with no uid cannot be judged by a uid policy, so it is
    /// refused rather than defaulted to anything.
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
}

// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Runtime side of the policy: holds the loaded [`Policy`], memoizes group
//! lookups, and answers one question — may this caller do this?

use std::sync::{Arc, RwLock};

use rsbinder::Caller;

use super::model::{Permission, Policy, Subject};
use crate::nss::GroupCache;

/// Applies a [`Policy`] to live callers.
///
/// Held behind a shared reference for the lifetime of `rsb_hub`; the
/// policy inside can be swapped by [`replace_policy`](Self::replace_policy)
/// on SIGHUP without disturbing in-flight transactions.
pub struct Enforcer {
    policy: RwLock<Arc<Policy>>,
    /// Set by `--insecure-allow-all`. Short-circuits every check.
    allow_all: bool,
    groups: GroupCache,
}

impl Enforcer {
    /// An enforcer that applies `policy`.
    pub fn enforcing(policy: Policy) -> Self {
        Enforcer {
            policy: RwLock::new(Arc::new(policy)),
            allow_all: false,
            groups: GroupCache::new(),
        }
    }

    /// An enforcer that permits everything — `--insecure-allow-all`.
    ///
    /// Only for development and for the integration suite, which spins up
    /// throwaway services under names no policy file could know in advance.
    /// It is a separate constructor rather than an empty policy so that
    /// "allow everything" can never be the result of a policy that merely
    /// failed to load.
    pub fn allow_all() -> Self {
        Enforcer {
            policy: RwLock::new(Arc::new(Policy::deny_all())),
            allow_all: true,
            groups: GroupCache::new(),
        }
    }

    /// Is this enforcer in `--insecure-allow-all` mode?
    pub fn is_allow_all(&self) -> bool {
        self.allow_all
    }

    /// Swap in a freshly loaded policy and drop the memoized group sets,
    /// so a reload also picks up group-membership changes.
    pub fn replace_policy(&self, policy: Policy) {
        *self.policy.write().expect("policy lock poisoned") = Arc::new(policy);
        self.groups.clear();
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
        let policy = Arc::clone(&*self.policy.read().expect("policy lock poisoned"));
        policy.check(permission, name, &self.subject_for(uid))
    }

    /// May `caller` exercise `permission` on `name`?
    ///
    /// Identities the policy cannot key on are denied: this model is
    /// uid-based, and a vsock cid is a routing address rather than a
    /// principal, a TLS certificate needs a certificate→principal mapping
    /// that does not exist here, and an anonymous peer has no identity at
    /// all. Denying is the only honest answer for those — see
    /// [`rsbinder::rpc::PeerIdentity`].
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
    use crate::policy::{NamePattern, Rule, Subjects};

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
        let enforcer = Enforcer::enforcing(policy_allowing_uid(1000));
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
        let enforcer = Enforcer::enforcing(policy_allowing_uid(1000));
        assert!(enforcer.check_uid(Permission::Find, "svc", 1000));
        enforcer.replace_policy(policy_allowing_uid(2000));
        assert!(!enforcer.check_uid(Permission::Find, "svc", 1000));
        assert!(enforcer.check_uid(Permission::Find, "svc", 2000));
    }

    /// A reload must not leave a permissive window: swapping in a
    /// deny-all policy denies immediately.
    #[test]
    fn replace_with_deny_all_denies() {
        let enforcer = Enforcer::enforcing(policy_allowing_uid(1000));
        enforcer.replace_policy(Policy::deny_all());
        assert!(!enforcer.check_uid(Permission::Add, "svc", 1000));
    }

    #[test]
    fn kernel_caller_is_keyed_on_uid() {
        let enforcer = Enforcer::enforcing(policy_allowing_uid(1000));
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
        let enforcer = Enforcer::enforcing(policy_allowing_uid(1000));
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

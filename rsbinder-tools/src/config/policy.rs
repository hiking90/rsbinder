// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The policy model and its evaluator. Pure: no file I/O, no NSS, no
//! binder. The TOML front end and the uid → groups resolver layer on top
//! of these types so the decision logic can be exhaustively unit-tested
//! without a filesystem or a user database.

use std::collections::BTreeSet;
use std::fmt;

/// The three permissions a service-manager policy governs, mirroring
/// AOSP's `Access::canAdd` / `canFind` / `canList`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Permission {
    /// Register (or replace, or unregister) a service under a name, and
    /// register a client callback for it. Gates `addService`,
    /// `registerClientCallback`, `tryUnregisterService`.
    Add,
    /// Look a name up, or ask anything about it. Gates `getService`,
    /// `checkService`, their `2` variants, `registerForNotifications`,
    /// `unregisterForNotifications`, `isDeclared`, `getConnectionInfo`,
    /// and the per-name filtering of `listServices`.
    Find,
    /// Enumerate names at all. A single global gate (there is no name to
    /// scope it to) on `listServices` and `getServiceDebugInfo`; the
    /// entries those return are additionally filtered by [`Self::Find`].
    List,
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Permission::Add => "add",
            Permission::Find => "find",
            Permission::List => "list",
        })
    }
}

/// A resolved caller: the uid the kernel vouched for, plus every gid that
/// uid belongs to (primary + supplementary), resolved through NSS by the
/// caller of this module.
///
/// Deliberately carries **no pid**. See the module docs: pid reuse makes
/// pid-derived attributes unsound as an authorization key, which is why
/// AOSP calls its own field `debugPid`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    /// Effective uid of the caller, as reported by the transport
    /// (kernel binder `sender_euid`, or `SO_PEERCRED`/`getpeereid` for
    /// Unix-socket RPC).
    pub uid: u32,
    /// Every group the uid belongs to.
    pub gids: BTreeSet<u32>,
}

impl Subject {
    /// A subject with no group memberships — the shape used by tests and
    /// by transports that cannot resolve groups.
    pub fn uid_only(uid: u32) -> Self {
        Subject {
            uid,
            gids: BTreeSet::new(),
        }
    }
}

/// Who a rule grants one permission to.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Subjects {
    /// Nobody. The default for every permission a rule does not mention,
    /// so an incomplete rule fails closed rather than inheriting.
    #[default]
    None,
    /// Any caller. Must be written out explicitly (`"any"`); it is never
    /// what an omitted key means.
    Any,
    /// The union of an explicit uid set and a gid set. A subject matches
    /// if its uid is listed, or if any of its gids is.
    Set {
        /// Effective uids granted this permission.
        uids: BTreeSet<u32>,
        /// Groups granted this permission; a caller matches if it belongs
        /// to any of them.
        gids: BTreeSet<u32>,
    },
}

impl Subjects {
    /// Build a `Set` from iterators of uids and gids, collapsing an empty
    /// result to [`Subjects::None`] so `{ uid = [], group = [] }` cannot
    /// masquerade as a grant.
    pub fn set(
        uids: impl IntoIterator<Item = u32>,
        gids: impl IntoIterator<Item = u32>,
    ) -> Subjects {
        let uids: BTreeSet<u32> = uids.into_iter().collect();
        let gids: BTreeSet<u32> = gids.into_iter().collect();
        if uids.is_empty() && gids.is_empty() {
            Subjects::None
        } else {
            Subjects::Set { uids, gids }
        }
    }

    /// Does `subject` fall in this set?
    pub fn allows(&self, subject: &Subject) -> bool {
        self.allows_parts(subject.uid, &subject.gids)
    }

    /// [`allows`](Self::allows) for a subject held as parts, so a borrowed group set needs no copy.
    pub(crate) fn allows_parts(&self, uid: u32, member_of: &BTreeSet<u32>) -> bool {
        match self {
            Subjects::None => false,
            Subjects::Any => true,
            Subjects::Set { uids, gids } => {
                uids.contains(&uid) || member_of.iter().any(|g| gids.contains(g))
            }
        }
    }
}

/// A service-name pattern.
///
/// Only a trailing `*` is supported. Service names are restricted to
/// `[A-Za-z0-9._/-]` by `rsb_hub`'s own `is_valid_service_name`, so a
/// general glob engine would buy nothing over a matcher small enough to
/// audit by eye — and an auditable matcher is the point in a file whose
/// job is to deny things.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamePattern {
    /// `"*"` — matches every name.
    Any,
    /// `"com.example.foo"` — matches exactly that name.
    Exact(String),
    /// `"com.example.*"` — matches any name with this prefix (the stored
    /// string excludes the `*`). An empty prefix is [`NamePattern::Any`].
    Prefix(String),
}

impl NamePattern {
    /// Parse a pattern from its policy-file spelling.
    ///
    /// Rejects an interior or leading `*`: `"*.foo"` and `"a*b"` look like
    /// they mean something and would silently match nothing, which in a
    /// default-deny policy is a hole that fails *open* for whatever later
    /// rule catches the name instead.
    pub fn parse(spec: &str) -> Result<NamePattern, PolicyError> {
        if spec.is_empty() {
            return Err(PolicyError::EmptyPattern);
        }
        match spec.find('*') {
            None => Ok(NamePattern::Exact(spec.to_owned())),
            Some(pos) if pos == spec.len() - 1 => {
                let prefix = &spec[..pos];
                if prefix.is_empty() {
                    Ok(NamePattern::Any)
                } else {
                    Ok(NamePattern::Prefix(prefix.to_owned()))
                }
            }
            Some(_) => Err(PolicyError::UnsupportedWildcard(spec.to_owned())),
        }
    }

    /// Does this pattern cover `name`?
    pub fn matches(&self, name: &str) -> bool {
        match self {
            NamePattern::Any => true,
            NamePattern::Exact(exact) => exact == name,
            NamePattern::Prefix(prefix) => name.starts_with(prefix.as_str()),
        }
    }
}

impl fmt::Display for NamePattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NamePattern::Any => f.write_str("*"),
            NamePattern::Exact(exact) => f.write_str(exact),
            NamePattern::Prefix(prefix) => write!(f, "{prefix}*"),
        }
    }
}

/// One rule: a name pattern plus who may `add` and who may `find` names
/// it covers. `list` is not here because it is global — there is no name
/// to scope it to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    /// Names this rule governs.
    pub pattern: NamePattern,
    /// Who may register under a covered name.
    pub add: Subjects,
    /// Who may look a covered name up.
    pub find: Subjects,
}

impl Rule {
    /// A rule that grants nothing — the base every builder starts from.
    pub fn deny_all(pattern: NamePattern) -> Rule {
        Rule {
            pattern,
            add: Subjects::None,
            find: Subjects::None,
        }
    }

    fn subjects_for(&self, permission: Permission) -> &Subjects {
        match permission {
            Permission::Add => &self.add,
            Permission::Find => &self.find,
            // `List` is global and never reaches a rule; see `Policy::check`.
            Permission::List => &Subjects::None,
        }
    }
}

/// A complete policy: the global `list` gate plus an ordered rule list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Policy {
    /// Who may enumerate service names at all.
    pub list: Subjects,
    /// Rules in evaluation order (policy files sorted by filename, rules
    /// in file order).
    pub rules: Vec<Rule>,
}

impl Policy {
    /// A policy that denies everything. Equivalent to `Policy::default()`,
    /// named so the intent reads at the call site.
    pub fn deny_all() -> Policy {
        Policy::default()
    }

    /// May `subject` exercise `permission` on `name`?
    ///
    /// **First matching rule decides.** The first rule whose pattern covers
    /// `name` answers the question and evaluation stops — a later rule
    /// never widens what an earlier one refused. That makes the governing
    /// rule for any name unique and greppable, which matters more in a
    /// security file than the extra expressiveness of D-Bus-style
    /// last-match-wins.
    ///
    /// No matching rule ⇒ deny. `name` is ignored for
    /// [`Permission::List`], which is answered by the global gate.
    pub fn check(&self, permission: Permission, name: &str, subject: &Subject) -> bool {
        self.check_parts(permission, name, subject.uid, &subject.gids)
    }

    /// [`check`](Self::check) for a subject held as parts, so a borrowed group set needs no copy.
    pub(crate) fn check_parts(
        &self,
        permission: Permission,
        name: &str,
        uid: u32,
        gids: &BTreeSet<u32>,
    ) -> bool {
        if permission == Permission::List {
            return self.list.allows_parts(uid, gids);
        }
        match self.rules.iter().find(|rule| rule.pattern.matches(name)) {
            Some(rule) => rule.subjects_for(permission).allows_parts(uid, gids),
            None => false,
        }
    }

    /// Could a larger group set change the answer to
    /// [`check(permission, name, _)`](Self::check)?
    ///
    /// True only when the governing subject set (the global `list` gate, or
    /// the first rule covering `name`) grants to at least one group. An
    /// `any` set has already allowed, and `none` or a uid-only set answers
    /// the same whatever the groups are. `Enforcer` uses this to decide
    /// whether a deny is worth a fresh name-service read.
    pub fn group_could_grant(&self, permission: Permission, name: &str) -> bool {
        let subjects = if permission == Permission::List {
            &self.list
        } else {
            match self.rules.iter().find(|rule| rule.pattern.matches(name)) {
                Some(rule) => rule.subjects_for(permission),
                None => return false,
            }
        };
        matches!(subjects, Subjects::Set { gids, .. } if !gids.is_empty())
    }
}

/// Why a policy could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PolicyError {
    /// A rule's `name` was the empty string.
    #[error("service name pattern must not be empty")]
    EmptyPattern,
    /// A `*` appeared anywhere but the end. See [`NamePattern::parse`].
    #[error("unsupported wildcard in {0:?}: only a trailing `*` is supported")]
    UnsupportedWildcard(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subject(uid: u32, gids: &[u32]) -> Subject {
        Subject {
            uid,
            gids: gids.iter().copied().collect(),
        }
    }

    #[test]
    fn pattern_parse_and_match() {
        assert_eq!(NamePattern::parse("*").unwrap(), NamePattern::Any);
        assert_eq!(
            NamePattern::parse("com.example.foo").unwrap(),
            NamePattern::Exact("com.example.foo".to_owned())
        );
        assert_eq!(
            NamePattern::parse("com.example.*").unwrap(),
            NamePattern::Prefix("com.example.".to_owned())
        );

        assert!(NamePattern::Any.matches("anything"));
        assert!(NamePattern::parse("com.example.*")
            .unwrap()
            .matches("com.example.foo"));
        // The prefix keeps the trailing dot, so a sibling namespace is not covered.
        assert!(!NamePattern::parse("com.example.*")
            .unwrap()
            .matches("com.examplefoo"));
        assert!(!NamePattern::parse("com.example.foo")
            .unwrap()
            .matches("com.example.foobar"));
    }

    /// A leading/interior `*` matches nothing and would hand the name to the next rule.
    #[test]
    fn pattern_rejects_non_trailing_wildcard() {
        assert_eq!(
            NamePattern::parse("*.foo"),
            Err(PolicyError::UnsupportedWildcard("*.foo".to_owned()))
        );
        assert_eq!(
            NamePattern::parse("a*b"),
            Err(PolicyError::UnsupportedWildcard("a*b".to_owned()))
        );
        assert_eq!(
            NamePattern::parse("com.*.foo"),
            Err(PolicyError::UnsupportedWildcard("com.*.foo".to_owned()))
        );
        assert_eq!(NamePattern::parse(""), Err(PolicyError::EmptyPattern));
    }

    #[test]
    fn subjects_match_by_uid_or_group() {
        let s = Subjects::set([1000], [50]);
        assert!(s.allows(&subject(1000, &[])));
        assert!(s.allows(&subject(2000, &[50])));
        // A supplementary group matches too, not just the primary one (getgrouplist).
        assert!(s.allows(&subject(2000, &[99, 50])));
        assert!(!s.allows(&subject(2000, &[99])));

        assert!(Subjects::Any.allows(&subject(0, &[])));
        assert!(!Subjects::None.allows(&subject(0, &[])));
    }

    /// `{ uid = [], group = [] }` is an empty grant, not a wildcard.
    #[test]
    fn empty_subject_set_collapses_to_none() {
        assert_eq!(Subjects::set([], []), Subjects::None);
        assert!(!Subjects::set([], []).allows(&subject(0, &[])));
    }

    /// The headline invariant: nothing is permitted unless a rule says so.
    #[test]
    fn empty_policy_denies_everything() {
        let policy = Policy::deny_all();
        let root = subject(0, &[0]);
        for perm in [Permission::Add, Permission::Find, Permission::List] {
            assert!(
                !policy.check(perm, "anything", &root),
                "{perm} must be denied by an empty policy, even for root"
            );
        }
    }

    /// A rule that mentions only `add` must not leak `find`.
    #[test]
    fn unmentioned_permission_denies() {
        let policy = Policy {
            list: Subjects::None,
            rules: vec![Rule {
                pattern: NamePattern::Any,
                add: Subjects::Any,
                find: Subjects::None,
            }],
        };
        let anyone = subject(1000, &[]);
        assert!(policy.check(Permission::Add, "svc", &anyone));
        assert!(!policy.check(Permission::Find, "svc", &anyone));
    }

    /// First match wins: the catch-all behind a specific rule never widens it.
    #[test]
    fn first_matching_rule_decides() {
        let policy = Policy {
            list: Subjects::None,
            rules: vec![
                Rule {
                    pattern: NamePattern::parse("com.example.*").unwrap(),
                    add: Subjects::set([1000], []),
                    find: Subjects::Any,
                },
                Rule {
                    pattern: NamePattern::Any,
                    add: Subjects::Any,
                    find: Subjects::Any,
                },
            ],
        };
        let other = subject(2000, &[]);
        // The first rule covers it and omits uid 2000; the catch-all must not rescue it.
        assert!(!policy.check(Permission::Add, "com.example.foo", &other));
        // Not covered by the first rule, so the catch-all applies.
        assert!(policy.check(Permission::Add, "org.other.foo", &other));
    }

    /// `list` is global and must not be answered by a name rule.
    #[test]
    fn list_uses_the_global_gate_only() {
        let policy = Policy {
            list: Subjects::set([], [50]),
            rules: vec![Rule {
                pattern: NamePattern::Any,
                add: Subjects::Any,
                find: Subjects::Any,
            }],
        };
        assert!(policy.check(Permission::List, "ignored", &subject(1000, &[50])));
        assert!(!policy.check(Permission::List, "ignored", &subject(1000, &[])));
    }

    /// `GroupCache` relies on this: more groups never turn an allow into a deny.
    #[test]
    fn policy_is_monotone_in_gids() {
        let group_sets: [&[u32]; 4] = [&[], &[50], &[50, 60], &[60]];
        let variants = [
            Subjects::None,
            Subjects::Any,
            Subjects::set([1], []),
            Subjects::set([], [50]),
            Subjects::set([1], [60]),
        ];
        let policy_for = |subjects: &Subjects| Policy {
            list: subjects.clone(),
            rules: vec![
                Rule {
                    pattern: NamePattern::parse("com.a.*").unwrap(),
                    add: subjects.clone(),
                    find: Subjects::set([2], []),
                },
                Rule {
                    pattern: NamePattern::Any,
                    add: Subjects::None,
                    find: subjects.clone(),
                },
            ],
        };
        for subjects in &variants {
            let policy = policy_for(subjects);
            for uid in [1, 2, 3] {
                for small in group_sets {
                    for large in group_sets {
                        if !small.iter().all(|g| large.contains(g)) {
                            continue;
                        }
                        let (s, l) = (subject(uid, small), subject(uid, large));
                        assert!(!subjects.allows(&s) || subjects.allows(&l));
                        for (perm, name) in [
                            (Permission::Add, "com.a.x"),
                            (Permission::Find, "com.a.x"),
                            (Permission::Find, "other"),
                            (Permission::List, "ignored"),
                        ] {
                            assert!(
                                !policy.check(perm, name, &s) || policy.check(perm, name, &l),
                                "{subjects:?} {perm} {name} uid {uid}: {small:?} ⊆ {large:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn group_could_grant_matches_the_governing_rule() {
        let mut policy = Policy {
            list: Subjects::Any,
            rules: vec![
                Rule {
                    pattern: NamePattern::parse("com.a.*").unwrap(),
                    add: Subjects::None,
                    find: Subjects::set([1], []),
                },
                Rule {
                    pattern: NamePattern::Any,
                    add: Subjects::None,
                    find: Subjects::set([], [50]),
                },
            ],
        };
        // The first rule covers it and grants to no group; the catch-all's group is irrelevant.
        assert!(!policy.group_could_grant(Permission::Find, "com.a.x"));
        assert!(policy.group_could_grant(Permission::Find, "other"));
        assert!(!policy.group_could_grant(Permission::Add, "other"));
        assert!(!policy.group_could_grant(Permission::List, "other"));
        policy.list = Subjects::set([], [50]);
        assert!(policy.group_could_grant(Permission::List, "other"));
        assert!(!Policy::deny_all().group_could_grant(Permission::Find, "other"));
    }
}

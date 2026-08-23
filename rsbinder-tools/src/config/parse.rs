// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! TOML front end for [`Policy`].
//!
//! ```toml
//! # /etc/rsbinder/hub.d/10-example.toml
//!
//! [global]
//! # Who may enumerate service names at all. Omitted ⇒ nobody.
//! list = { group = ["binder-admin"] }
//!
//! [[rule]]
//! name = "com.example.*"          # trailing `*` only
//! add  = { user = ["exampled"] }
//! find = { group = ["example-clients"], uid = [0] }
//!
//! [[rule]]
//! name = "*"
//! add  = "none"
//! find = "none"
//! ```
//!
//! A directory is read as every `*.toml` in it, sorted by file name, and
//! the rules are concatenated in that order — so `10-`/`20-` prefixes
//! control precedence the way they do in any other `.d` directory.
//! Because the evaluator stops at the **first** matching rule
//! ([`Policy::check`]), a catch-all belongs in the highest-numbered file.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::policy::{NamePattern, Policy, PolicyError, Rule, Subjects};
use crate::nss::{self, NssError};

/// Resolves user and group names to numeric ids.
///
/// Injectable so the whole config path can be unit-tested against names
/// that do not exist on the build machine — a policy file naturally names
/// site-specific accounts, and a test that needed them to be real would
/// only ever run on one host.
pub trait NameResolver {
    /// User name → uid.
    fn uid(&self, name: &str) -> Result<u32, NssError>;
    /// Group name (or numeric gid) → gid.
    fn gid(&self, name: &str) -> Result<u32, NssError>;
}

/// The real name service.
pub struct SystemResolver;

impl NameResolver for SystemResolver {
    fn uid(&self, name: &str) -> Result<u32, NssError> {
        nss::uid_for_user(name)
    }
    fn gid(&self, name: &str) -> Result<u32, NssError> {
        nss::gid_for_group(name)
    }
}

/// Why a policy could not be loaded.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The policy path could not be read.
    #[error("cannot read policy path {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The policy directory contained no `*.toml` files. Not silently
    /// treated as deny-all: an operator who meant deny-all writes it, and
    /// an empty directory is far more often a deployment mistake.
    #[error("no policy files (*.toml) found in {0}")]
    NoPolicyFiles(PathBuf),
    /// A file was not valid TOML, or had unknown/mistyped keys.
    #[error("{path}: {source}")]
    Toml {
        /// The offending file.
        path: PathBuf,
        /// The parse error, including line and column.
        source: toml::de::Error,
    },
    /// A rule's `name` pattern was rejected.
    #[error("{path}: rule {index}: {source}")]
    Pattern {
        /// The offending file.
        path: PathBuf,
        /// Zero-based index of the rule within its file.
        index: usize,
        /// Why the pattern was rejected.
        source: PolicyError,
    },
    /// A user or group named in the policy could not be resolved.
    #[error("{path}: {source}")]
    Name {
        /// The offending file.
        path: PathBuf,
        /// The lookup failure.
        source: NssError,
    },
    /// `[global] list` was set in more than one file. Ambiguity in a
    /// security file is an error, not a last-one-wins.
    #[error("[global] list is defined in both {first} and {second}; it may only be set once")]
    DuplicateGlobal {
        /// The file that set it first.
        first: PathBuf,
        /// The file that set it again.
        second: PathBuf,
    },
    /// A keyword subject other than `"any"` / `"none"`.
    #[error("{path}: unknown subject keyword {keyword:?} (expected \"any\" or \"none\")")]
    UnknownKeyword {
        /// The offending file.
        path: PathBuf,
        /// What was written.
        keyword: String,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    #[serde(default)]
    global: Option<RawGlobal>,
    #[serde(default)]
    rule: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGlobal {
    #[serde(default)]
    list: Option<RawSubjects>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRule {
    name: String,
    #[serde(default)]
    add: Option<RawSubjects>,
    #[serde(default)]
    find: Option<RawSubjects>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum RawSubjects {
    Keyword(String),
    Set(RawSubjectSet),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSubjectSet {
    #[serde(default)]
    uid: Vec<u32>,
    #[serde(default)]
    user: Vec<String>,
    #[serde(default)]
    group: Vec<String>,
}

impl RawSubjects {
    fn resolve(self, path: &Path, resolver: &dyn NameResolver) -> Result<Subjects, ConfigError> {
        match self {
            RawSubjects::Keyword(word) => match word.as_str() {
                "any" => Ok(Subjects::Any),
                "none" => Ok(Subjects::None),
                _ => Err(ConfigError::UnknownKeyword {
                    path: path.to_owned(),
                    keyword: word,
                }),
            },
            RawSubjects::Set(set) => {
                let mut uids: BTreeSet<u32> = set.uid.into_iter().collect();
                for user in set.user {
                    uids.insert(resolver.uid(&user).map_err(|source| ConfigError::Name {
                        path: path.to_owned(),
                        source,
                    })?);
                }
                let mut gids = BTreeSet::new();
                for group in set.group {
                    gids.insert(resolver.gid(&group).map_err(|source| ConfigError::Name {
                        path: path.to_owned(),
                        source,
                    })?);
                }
                Ok(Subjects::set(uids, gids))
            }
        }
    }
}

/// Parse one policy file's text into a global gate (if it sets one) and
/// its rules, resolving every name through `resolver`.
///
/// `path` is used only for error messages.
pub fn parse_file(
    path: &Path,
    text: &str,
    resolver: &dyn NameResolver,
) -> Result<(Option<Subjects>, Vec<Rule>), ConfigError> {
    let raw: RawFile = toml::from_str(text).map_err(|source| ConfigError::Toml {
        path: path.to_owned(),
        source,
    })?;

    let global = match raw.global.and_then(|g| g.list) {
        Some(list) => Some(list.resolve(path, resolver)?),
        None => None,
    };

    let mut rules = Vec::with_capacity(raw.rule.len());
    for (index, raw_rule) in raw.rule.into_iter().enumerate() {
        let pattern =
            NamePattern::parse(&raw_rule.name).map_err(|source| ConfigError::Pattern {
                path: path.to_owned(),
                index,
                source,
            })?;
        rules.push(Rule {
            pattern,
            // An omitted permission denies. Nothing is inherited from a
            // previous rule or from `[global]`.
            add: match raw_rule.add {
                Some(s) => s.resolve(path, resolver)?,
                None => Subjects::None,
            },
            find: match raw_rule.find {
                Some(s) => s.resolve(path, resolver)?,
                None => Subjects::None,
            },
        });
    }
    Ok((global, rules))
}

/// Every `*.toml` under `dir`, sorted by file name.
fn policy_files(dir: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    let entries = std::fs::read_dir(dir).map_err(|source| ConfigError::Io {
        path: dir.to_owned(),
        source,
    })?;
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ConfigError::Io {
            path: dir.to_owned(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "toml") && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Load a policy from a single file or from a directory of `*.toml`.
///
/// Fails rather than degrading: an unreadable path, an empty directory, a
/// malformed file, or an unresolvable user/group all return an error, and
/// `rsb_hub` refuses to start on any of them. There is no partial policy —
/// a policy that silently dropped the rule it could not parse would be a
/// policy that fails open.
pub fn load(path: &Path, resolver: &dyn NameResolver) -> Result<Policy, ConfigError> {
    let files = if path.is_dir() {
        let files = policy_files(path)?;
        if files.is_empty() {
            return Err(ConfigError::NoPolicyFiles(path.to_owned()));
        }
        files
    } else {
        vec![path.to_owned()]
    };

    let mut policy = Policy::deny_all();
    let mut global_origin: Option<PathBuf> = None;

    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|source| ConfigError::Io {
            path: file.clone(),
            source,
        })?;
        let (global, rules) = parse_file(&file, &text, resolver)?;
        if let Some(global) = global {
            if let Some(first) = &global_origin {
                return Err(ConfigError::DuplicateGlobal {
                    first: first.clone(),
                    second: file.clone(),
                });
            }
            policy.list = global;
            global_origin = Some(file.clone());
        }
        policy.rules.extend(rules);
    }

    Ok(policy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Permission, Subject};

    /// Deterministic stand-in for the name service: `"svcuser"` is uid
    /// 1000 and `"svcgroup"` is gid 50, on every machine.
    struct FakeResolver;

    impl NameResolver for FakeResolver {
        fn uid(&self, name: &str) -> Result<u32, NssError> {
            match name {
                "svcuser" => Ok(1000),
                "root" => Ok(0),
                _ => Err(NssError::NotFound {
                    kind: "user",
                    name: name.to_owned(),
                }),
            }
        }
        fn gid(&self, name: &str) -> Result<u32, NssError> {
            match name {
                "svcgroup" => Ok(50),
                "admin" => Ok(51),
                _ => Err(NssError::NotFound {
                    kind: "group",
                    name: name.to_owned(),
                }),
            }
        }
    }

    fn parse(text: &str) -> Result<Policy, ConfigError> {
        let (global, rules) = parse_file(Path::new("test.toml"), text, &FakeResolver)?;
        Ok(Policy {
            list: global.unwrap_or(Subjects::None),
            rules,
        })
    }

    fn subject(uid: u32, gids: &[u32]) -> Subject {
        Subject {
            uid,
            gids: gids.iter().copied().collect(),
        }
    }

    #[test]
    fn full_example_parses_and_evaluates() {
        let policy = parse(
            r#"
            [global]
            list = { group = ["admin"] }

            [[rule]]
            name = "com.example.*"
            add  = { user = ["svcuser"] }
            find = { group = ["svcgroup"], uid = [0] }

            [[rule]]
            name = "*"
            add  = "none"
            find = "none"
            "#,
        )
        .expect("policy must parse");

        let svc = subject(1000, &[]);
        let client = subject(2000, &[50]);
        let stranger = subject(3000, &[]);
        let admin = subject(4000, &[51]);

        assert!(policy.check(Permission::Add, "com.example.foo", &svc));
        assert!(!policy.check(Permission::Add, "com.example.foo", &client));
        assert!(policy.check(Permission::Find, "com.example.foo", &client));
        assert!(policy.check(Permission::Find, "com.example.foo", &subject(0, &[])));
        assert!(!policy.check(Permission::Find, "com.example.foo", &stranger));

        // The catch-all denies everything outside com.example.
        assert!(!policy.check(Permission::Add, "org.other", &svc));
        assert!(!policy.check(Permission::Find, "org.other", &client));

        assert!(policy.check(Permission::List, "", &admin));
        assert!(!policy.check(Permission::List, "", &svc));
    }

    /// `[global]` absent ⇒ nobody may enumerate.
    #[test]
    fn missing_global_denies_list() {
        let policy = parse("[[rule]]\nname = \"*\"\nfind = \"any\"\n").unwrap();
        assert!(!policy.check(Permission::List, "", &subject(0, &[0])));
        assert!(policy.check(Permission::Find, "x", &subject(0, &[0])));
    }

    /// A typo in a key must be an error, never a silently ignored grant.
    #[test]
    fn unknown_keys_are_rejected() {
        let err = parse("[[rule]]\nname = \"*\"\nadd = \"any\"\nfnid = \"any\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "got {err:?}");

        let err = parse("[[rule]]\nname = \"*\"\nadd = { users = [\"svcuser\"] }\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "got {err:?}");

        let err = parse("[gobal]\nlist = \"any\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "got {err:?}");
    }

    #[test]
    fn unknown_keyword_is_rejected() {
        let err = parse("[[rule]]\nname = \"*\"\nadd = \"all\"\n").unwrap_err();
        match err {
            ConfigError::UnknownKeyword { keyword, .. } => assert_eq!(keyword, "all"),
            other => panic!("got {other:?}"),
        }
    }

    #[test]
    fn unresolvable_name_is_rejected() {
        let err =
            parse("[[rule]]\nname = \"*\"\nadd = { user = [\"nobody-here\"] }\n").unwrap_err();
        assert!(matches!(err, ConfigError::Name { .. }), "got {err:?}");
    }

    #[test]
    fn bad_pattern_is_rejected_with_its_index() {
        let err = parse("[[rule]]\nname = \"a\"\n\n[[rule]]\nname = \"*.bad\"\n").unwrap_err();
        match err {
            ConfigError::Pattern { index, source, .. } => {
                assert_eq!(index, 1);
                assert!(matches!(source, PolicyError::UnsupportedWildcard(_)));
            }
            other => panic!("got {other:?}"),
        }
    }

    /// Numeric ids must work with no name service at all.
    #[test]
    fn numeric_ids_need_no_resolver() {
        let policy = parse("[[rule]]\nname = \"*\"\nadd = { uid = [1234] }\n").unwrap();
        assert!(policy.check(Permission::Add, "x", &subject(1234, &[])));
        assert!(!policy.check(Permission::Add, "x", &subject(1235, &[])));
    }

    /// Directory load: file-name order decides rule precedence, and the
    /// first match wins — so `10-` beats the catch-all in `99-`.
    #[test]
    fn directory_load_orders_by_file_name() {
        let dir = std::env::temp_dir().join(format!("rsb-policy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("99-catchall.toml"),
            "[[rule]]\nname = \"*\"\nadd = \"any\"\nfind = \"any\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("10-specific.toml"),
            "[global]\nlist = \"any\"\n\n[[rule]]\nname = \"locked.*\"\nadd = \"none\"\nfind = \"none\"\n",
        )
        .unwrap();
        // Not a .toml — must be ignored, not parsed.
        std::fs::write(dir.join("README"), "not toml at all {{{").unwrap();

        let policy = load(&dir, &FakeResolver).expect("directory must load");
        let anyone = subject(1234, &[]);
        assert!(!policy.check(Permission::Add, "locked.foo", &anyone));
        assert!(policy.check(Permission::Add, "open.foo", &anyone));
        assert!(policy.check(Permission::List, "", &anyone));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_directory_is_an_error_not_deny_all() {
        let dir = std::env::temp_dir().join(format!("rsb-policy-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            load(&dir, &FakeResolver),
            Err(ConfigError::NoPolicyFiles(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn duplicate_global_is_an_error() {
        let dir = std::env::temp_dir().join(format!("rsb-policy-dup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("10-a.toml"), "[global]\nlist = \"any\"\n").unwrap();
        std::fs::write(dir.join("20-b.toml"), "[global]\nlist = \"none\"\n").unwrap();
        assert!(matches!(
            load(&dir, &FakeResolver),
            Err(ConfigError::DuplicateGlobal { .. })
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

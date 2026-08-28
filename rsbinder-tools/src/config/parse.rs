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
//!
//! # Declarations name concrete instances, not patterns. One entry says the
//! # instance exists (`isDeclared`), which interface it belongs to
//! # (`getDeclaredInstances`), how to bring it up on demand, and what
//! # `getConnectionInfo` should report.
//! [[service]]
//! name = "com.example.IFoo/default"
//! start = { systemd = "example-foo.service" }
//! # start = { exec = ["/usr/bin/exampled", "--instance", "default"] }
//! connection = { ip = "127.0.0.1", port = 8080 }
//! ```
//!
//! A directory is read as every `*.toml` in it, sorted by file name, and
//! the rules are concatenated in that order — so `10-`/`20-` prefixes
//! control precedence the way they do in any other `.d` directory.
//! Because the evaluator stops at the **first** matching rule
//! ([`Policy::check`]), a catch-all belongs in the highest-numbered file.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use super::declaration::{Activation, ConnectionInfo, Declaration, Declarations, InstanceName};
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
    /// The configuration path could not be read.
    #[error("cannot read configuration path {path}: {source}")]
    Io {
        /// The path that could not be read.
        path: PathBuf,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The directory contained no `*.toml` files. Not silently treated as
    /// deny-all: an operator who meant deny-all writes it, and an empty
    /// directory is far more often a deployment mistake.
    #[error("no configuration files (*.toml) found in {0}")]
    NoConfigFiles(PathBuf),
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
    /// The same instance is declared in two places. Ambiguity about what
    /// exists — and, with `start`, about what runs — is an error.
    #[error("service {name:?} is declared in both {first} and {second}")]
    DuplicateService {
        /// The instance declared twice.
        name: String,
        /// Where it was declared first.
        first: PathBuf,
        /// Where it was declared again.
        second: PathBuf,
    },
    /// A `start` table naming neither `systemd` nor `exec`, or both.
    #[error("{path}: service {name:?}: `start` must set exactly one of `systemd` or `exec`")]
    BadActivation {
        /// The offending file.
        path: PathBuf,
        /// The instance whose `start` is malformed.
        name: String,
    },
    /// An `exec` activation with no program to run.
    #[error("{path}: service {name:?}: `exec` must not be empty")]
    EmptyExec {
        /// The offending file.
        path: PathBuf,
        /// The instance whose `exec` is empty.
        name: String,
    },
    /// A configuration path anyone but root or this process can rewrite.
    /// See [`TrustProblem`](super::TrustProblem).
    #[error("{path} is {problem}; refusing to load configuration from it")]
    Untrusted {
        /// The offending path.
        path: PathBuf,
        /// What is wrong with it.
        problem: super::trust::TrustProblem,
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
    #[serde(default)]
    service: Vec<RawService>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawService {
    name: String,
    #[serde(default)]
    start: Option<RawActivation>,
    #[serde(default)]
    connection: Option<RawConnection>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawActivation {
    #[serde(default)]
    systemd: Option<String>,
    #[serde(default)]
    exec: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConnection {
    ip: String,
    port: i32,
}

impl RawService {
    fn resolve(self, path: &Path) -> Result<Declaration, ConfigError> {
        let activation = match self.start {
            None => None,
            Some(RawActivation {
                systemd: Some(unit),
                exec: None,
            }) => Some(Activation::Systemd(unit)),
            Some(RawActivation {
                systemd: None,
                exec: Some(argv),
            }) => {
                if argv.is_empty() {
                    return Err(ConfigError::EmptyExec {
                        path: path.to_owned(),
                        name: self.name,
                    });
                }
                Some(Activation::Exec(argv))
            }
            // Neither or both: refuse rather than pick one. What starts a
            // service is not somewhere to guess.
            Some(_) => {
                return Err(ConfigError::BadActivation {
                    path: path.to_owned(),
                    name: self.name,
                })
            }
        };
        Ok(Declaration {
            name: InstanceName::parse(&self.name),
            activation,
            connection: self.connection.map(|c| ConnectionInfo {
                ip: c.ip,
                port: c.port,
            }),
        })
    }
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

/// One file's contribution to the configuration.
pub struct FileContents {
    /// The `[global] list` gate, if this file sets one.
    pub global_list: Option<Subjects>,
    /// Access rules, in file order.
    pub rules: Vec<Rule>,
    /// Service declarations, paired with the full instance name they were
    /// written under.
    pub services: Vec<(String, Declaration)>,
}

/// Parse one configuration file, resolving every user and group name
/// through `resolver`.
///
/// `path` is used only for error messages.
pub fn parse_file(
    path: &Path,
    text: &str,
    resolver: &dyn NameResolver,
) -> Result<FileContents, ConfigError> {
    let raw: RawFile = toml::from_str(text).map_err(|source| ConfigError::Toml {
        path: path.to_owned(),
        source,
    })?;

    let global = match raw.global.and_then(|g| g.list) {
        Some(list) => Some(list.resolve(path, resolver)?),
        None => None,
    };

    let mut services = Vec::with_capacity(raw.service.len());
    for raw_service in raw.service {
        let full_name = raw_service.name.clone();
        services.push((full_name, raw_service.resolve(path)?));
    }

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
    Ok(FileContents {
        global_list: global,
        rules,
        services,
    })
}

/// Refuse a path anyone else can rewrite. See [`super::trust`] for why
/// this is fatal rather than a warning.
fn check_trusted(path: &Path, our_uid: u32) -> Result<(), ConfigError> {
    match super::trust::check_path(path, our_uid) {
        Ok(None) => Ok(()),
        Ok(Some(bad)) => Err(ConfigError::Untrusted {
            path: bad.path,
            problem: bad.problem,
        }),
        Err(source) => Err(ConfigError::Io {
            path: path.to_owned(),
            source,
        }),
    }
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

/// Everything one configuration directory says.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    /// Who may do what.
    pub policy: Policy,
    /// What services exist, and how to start them.
    pub declarations: Declarations,
}

/// Load the configuration from a single file or from a directory of `*.toml`.
///
/// Fails rather than degrading: an unreadable path, an empty directory, a
/// malformed file, or an unresolvable user/group all return an error, and
/// `rsb_hub` refuses to start on any of them. There is no partial policy —
/// a policy that silently dropped the rule it could not parse would be a
/// policy that fails open.
pub fn load(path: &Path, resolver: &dyn NameResolver) -> Result<Config, ConfigError> {
    let our_uid = rustix::process::getuid().as_raw();
    check_trusted(path, our_uid)?;

    let files = if path.is_dir() {
        let files = policy_files(path)?;
        if files.is_empty() {
            return Err(ConfigError::NoConfigFiles(path.to_owned()));
        }
        files
    } else {
        vec![path.to_owned()]
    };

    let mut config = Config::default();
    let mut global_origin: Option<PathBuf> = None;
    let mut service_origin: BTreeMap<String, PathBuf> = BTreeMap::new();

    for file in files {
        check_trusted(&file, our_uid)?;
        let text = std::fs::read_to_string(&file).map_err(|source| ConfigError::Io {
            path: file.clone(),
            source,
        })?;
        let contents = parse_file(&file, &text, resolver)?;
        if let Some(global) = contents.global_list {
            if let Some(first) = &global_origin {
                return Err(ConfigError::DuplicateGlobal {
                    first: first.clone(),
                    second: file.clone(),
                });
            }
            config.policy.list = global;
            global_origin = Some(file.clone());
        }
        config.policy.rules.extend(contents.rules);
        for (name, declaration) in contents.services {
            if let Some(first) = service_origin.get(&name) {
                return Err(ConfigError::DuplicateService {
                    name,
                    first: first.clone(),
                    second: file.clone(),
                });
            }
            service_origin.insert(name.clone(), file.clone());
            config.declarations.insert(name, declaration);
        }
    }

    Ok(config)
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
        let c = parse_file(Path::new("test.toml"), text, &FakeResolver)?;
        Ok(Policy {
            list: c.global_list.unwrap_or(Subjects::None),
            rules: c.rules,
        })
    }

    /// Parse the declaration half of a single file.
    fn parse_services(text: &str) -> Result<Declarations, ConfigError> {
        let c = parse_file(Path::new("test.toml"), text, &FakeResolver)?;
        let mut d = Declarations::default();
        for (name, decl) in c.services {
            d.insert(name, decl);
        }
        Ok(d)
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

        let policy = load(&dir, &FakeResolver)
            .expect("directory must load")
            .policy;
        let anyone = subject(1234, &[]);
        assert!(!policy.check(Permission::Add, "locked.foo", &anyone));
        assert!(policy.check(Permission::Add, "open.foo", &anyone));
        assert!(policy.check(Permission::List, "", &anyone));

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn service_declarations_parse() {
        let d = parse_services(
            r#"
            [[service]]
            name = "com.example.IFoo/default"
            start = { systemd = "example-foo.service" }
            connection = { ip = "127.0.0.1", port = 8080 }

            [[service]]
            name = "com.example.IFoo/secondary"
            start = { exec = ["/usr/bin/exampled", "--instance", "secondary"] }

            [[service]]
            name = "com.example.IBar/default"
            "#,
        )
        .expect("declarations must parse");

        assert!(d.is_declared("com.example.IFoo/default"));
        assert_eq!(
            d.instances_of("com.example.IFoo"),
            vec!["default".to_owned(), "secondary".to_owned()]
        );
        assert_eq!(
            d.connection_info("com.example.IFoo/default"),
            Some(&ConnectionInfo {
                ip: "127.0.0.1".to_owned(),
                port: 8080
            })
        );
        assert_eq!(
            d.activation("com.example.IFoo/default"),
            Some(&Activation::Systemd("example-foo.service".to_owned()))
        );
        assert!(matches!(
            d.activation("com.example.IFoo/secondary"),
            Some(Activation::Exec(argv)) if argv[0] == "/usr/bin/exampled"
        ));
        // Declared but not startable: `isDeclared` still answers true, which
        // is the point — the client can tell "not installed" from "not
        // started", even where rsb_hub cannot start it.
        assert!(d.is_declared("com.example.IBar/default"));
        assert!(d.activation("com.example.IBar/default").is_none());
    }

    /// What starts a service is not somewhere to guess.
    #[test]
    fn ambiguous_or_empty_activation_is_rejected() {
        let both = parse_services(
            "[[service]]\nname = \"a/b\"\nstart = { systemd = \"u.service\", exec = [\"/bin/true\"] }\n",
        )
        .unwrap_err();
        assert!(
            matches!(both, ConfigError::BadActivation { .. }),
            "{both:?}"
        );

        let neither = parse_services("[[service]]\nname = \"a/b\"\nstart = {}\n").unwrap_err();
        assert!(
            matches!(neither, ConfigError::BadActivation { .. }),
            "{neither:?}"
        );

        let empty =
            parse_services("[[service]]\nname = \"a/b\"\nstart = { exec = [] }\n").unwrap_err();
        assert!(matches!(empty, ConfigError::EmptyExec { .. }), "{empty:?}");
    }

    #[test]
    fn unknown_service_keys_are_rejected() {
        let err = parse_services("[[service]]\nname = \"a/b\"\nstrat = \"x\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "{err:?}");
    }

    /// Two files declaring the same instance is ambiguity about what runs.
    #[test]
    fn duplicate_service_declaration_is_an_error() {
        let dir = std::env::temp_dir().join(format!("rsb-cfg-dupsvc-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("10-a.toml"),
            "[[service]]\nname = \"a/b\"\nstart = { systemd = \"one.service\" }\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("20-b.toml"),
            "[[service]]\nname = \"a/b\"\nstart = { systemd = \"two.service\" }\n",
        )
        .unwrap();
        let err = load(&dir, &FakeResolver).unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicateService { ref name, .. } if name == "a/b"),
            "{err:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// Rules and declarations coexist in one file and load independently.
    #[test]
    fn rules_and_declarations_share_a_file() {
        let c = parse_file(
            Path::new("test.toml"),
            r#"
            [[rule]]
            name = "com.example.*"
            find = "any"

            [[service]]
            name = "com.example.IFoo/default"
            "#,
            &FakeResolver,
        )
        .expect("must parse");
        assert_eq!(c.rules.len(), 1);
        assert_eq!(c.services.len(), 1);
    }

    #[test]
    fn empty_directory_is_an_error_not_deny_all() {
        let dir = std::env::temp_dir().join(format!("rsb-policy-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(matches!(
            load(&dir, &FakeResolver),
            Err(ConfigError::NoConfigFiles(_))
        ));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// The configuration decides what runs; a file anyone can rewrite is a
    /// file anyone can use to run it.
    #[test]
    fn a_world_writable_directory_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("rsb-cfg-perm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("10.toml"),
            "[[rule]]\nname = \"*\"\nfind = \"any\"\n",
        )
        .unwrap();

        // Sane to begin with.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(load(&dir, &FakeResolver).is_ok());

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o777)).unwrap();
        let err = load(&dir, &FakeResolver).unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err:?}");

        // And a sane directory holding a writable file is refused too.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::set_permissions(dir.join("10.toml"), std::fs::Permissions::from_mode(0o666))
            .unwrap();
        let err = load(&dir, &FakeResolver).unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err:?}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
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

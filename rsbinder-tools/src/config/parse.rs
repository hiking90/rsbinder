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
//! A directory is read as every `*.toml` in it whose name does not start
//! with `.` (editor lock and backup files), sorted by file name, and
//! the rules are concatenated in that order — so `10-`/`20-` prefixes
//! control precedence the way they do in any other `.d` directory.
//! Because the evaluator stops at the **first** matching rule
//! ([`Policy::check`]), a catch-all belongs in the highest-numbered file.

use std::collections::{BTreeMap, BTreeSet};
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};

use rustix::fs::FileType;
use serde::Deserialize;

use super::declaration::{
    is_valid_service_name, Activation, ConnectionInfo, Declaration, Declarations, InstanceName,
};
use super::policy::{NamePattern, Policy, PolicyError, Rule, Subjects};
use super::trust::{At, Trusted};
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
    /// A `systemd` activation with no unit to start.
    #[error("{path}: service {name:?}: `systemd` unit must not be empty")]
    EmptyUnit {
        /// The offending file.
        path: PathBuf,
        /// The instance whose `systemd` unit is empty.
        name: String,
    },
    /// An `exec` whose program is not an absolute path. A relative one would
    /// be resolved through rsb_hub's `PATH` or working directory, and run
    /// with rsb_hub's privileges.
    #[error("{path}: service {name:?}: `exec` program {program:?} must be an absolute path")]
    RelativeExec {
        /// The offending file.
        path: PathBuf,
        /// The instance whose `exec` is relative.
        name: String,
        /// The program as written.
        program: String,
    },
    /// An `exec` program that someone other than root or this process can
    /// replace: the program, a directory on the way to it from `/`, or a
    /// symlink's directory fails the check [`Untrusted`](Self::Untrusted)
    /// applies to the configuration. The program runs with rsb_hub's
    /// privileges, so it is held to the same rule as the file that names it.
    ///
    /// Checked when the configuration is loaded, and again by
    /// [`SystemRunner`](super::SystemRunner) right before each start, which
    /// refuses a start whose program now fails (a SIGHUP reload that fails
    /// keeps the previous declarations). On Linux and Android only `x` is
    /// needed on the program, not `r`; on other OSes, which have no `O_PATH`,
    /// the check opens the program for reading, so it needs `r` as well. The
    /// start executes the path after its check, so a
    /// replacement made between the two is not caught; a path that passed
    /// can only be replaced by root or by rsb_hub's own uid.
    #[error(
        "{path}: service {name:?}: `exec` program {program:?} is not trusted: \
         {offender} is {problem}"
    )]
    UntrustedExec {
        /// The configuration file naming the program.
        path: PathBuf,
        /// The instance whose `exec` is untrusted.
        name: String,
        /// The program as written.
        program: String,
        /// The path that failed the check: the program or a directory above it.
        offender: PathBuf,
        /// What is wrong with it.
        problem: super::trust::TrustProblem,
    },
    /// An `exec` program whose trust could not be checked: it does not
    /// exist, or a component on the way to it cannot be opened. Refused
    /// rather than skipped, as an unopenable configuration entry is.
    #[error("{path}: service {name:?}: cannot check `exec` program {program:?}: {source}")]
    ExecIo {
        /// The configuration file naming the program.
        path: PathBuf,
        /// The instance whose `exec` could not be checked.
        name: String,
        /// The program as written.
        program: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// A `[[service]]` name that is not `interface/instance` with both halves
    /// non-empty, or that `addService` would refuse (see
    /// [`is_valid_service_name`]). There is no
    /// implicit instance: AOSP `AidlName::fill` likewise refuses a name without `/`.
    #[error(
        "{path}: service {name:?}: name must be `interface/instance`, both parts non-empty, \
         at most 127 bytes of [A-Za-z0-9._/-]"
    )]
    BadServiceName {
        /// The offending file.
        path: PathBuf,
        /// The name as written.
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
    // A port no client could dial (0, negative, above 65535) fails the load, not the dial.
    port: std::num::NonZeroU16,
}

impl RawService {
    fn resolve(self, path: &Path) -> Result<Declaration, ConfigError> {
        let parsed = InstanceName::parse(&self.name).filter(|_| is_valid_service_name(&self.name));
        let Some(name) = parsed else {
            return Err(ConfigError::BadServiceName {
                path: path.to_owned(),
                name: self.name,
            });
        };
        let activation = match self.start {
            None => None,
            Some(RawActivation {
                systemd: Some(unit),
                exec: None,
            }) => {
                if unit.is_empty() {
                    return Err(ConfigError::EmptyUnit {
                        path: path.to_owned(),
                        name: self.name,
                    });
                }
                Some(Activation::Systemd(unit))
            }
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
                if !Path::new(&argv[0]).is_absolute() {
                    return Err(ConfigError::RelativeExec {
                        path: path.to_owned(),
                        name: self.name,
                        program: argv[0].clone(),
                    });
                }
                Some(Activation::Exec(argv))
            }
            // Neither or both: refuse rather than guess what starts the service.
            Some(_) => {
                return Err(ConfigError::BadActivation {
                    path: path.to_owned(),
                    name: self.name,
                })
            }
        };
        Ok(Declaration {
            name,
            activation,
            connection: self.connection.map(|c| ConnectionInfo {
                ip: c.ip,
                port: c.port.get().into(),
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
    /// Service declarations, paired with the full instance name they were written under.
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
            // An omitted permission denies; nothing inherits from earlier rules or `[global]`.
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

/// Walk `rel` from `start`, refusing what others can rewrite or redirect; see [`super::trust`].
fn check_trusted(
    root: At<'_>,
    start: At<'_>,
    rel: &Path,
    given: &Path,
    our_uid: u32,
) -> Result<Trusted, ConfigError> {
    let io = |source| ConfigError::Io {
        path: given.to_owned(),
        source,
    };
    let untrusted = |bad: super::trust::Untrusted| ConfigError::Untrusted {
        path: bad.path,
        problem: bad.problem,
    };
    super::trust::check_from(root, start, rel, our_uid, super::trust::Use::Read)
        .map_err(io)?
        .map_err(untrusted)
}

/// Walk an `exec` program from `root` as the configuration itself is walked.
fn check_exec(
    root: At<'_>,
    file: &Path,
    name: &str,
    program: &str,
    our_uid: u32,
) -> Result<(), ConfigError> {
    match super::trust::check_program(root, Path::new(program), our_uid) {
        Ok(Ok(())) => Ok(()),
        Ok(Err(bad)) => Err(ConfigError::UntrustedExec {
            path: file.to_owned(),
            name: name.to_owned(),
            program: program.to_owned(),
            offender: bad.path,
            problem: bad.problem,
        }),
        Err(source) => Err(ConfigError::ExecIo {
            path: file.to_owned(),
            name: name.to_owned(),
            program: program.to_owned(),
            source,
        }),
    }
}

/// Read the checked inode through the descriptor that was checked.
fn read_trusted(entry: Trusted) -> Result<String, ConfigError> {
    use std::io::Read;
    let Trusted { fd, shown, .. } = entry;
    let mut text = String::new();
    std::fs::File::from(fd)
        .read_to_string(&mut text)
        .map_err(|source| ConfigError::Io {
            path: shown,
            source,
        })?;
    Ok(text)
}

/// Every regular, non-hidden `*.toml` in the held directory `dir`, sorted by file name.
fn policy_files(root: At<'_>, dir: &Trusted, our_uid: u32) -> Result<Vec<Trusted>, ConfigError> {
    use std::os::unix::ffi::OsStringExt;
    let io = |source: rustix::io::Errno| ConfigError::Io {
        path: dir.shown.clone(),
        source: source.into(),
    };
    let mut names = Vec::new();
    for entry in rustix::fs::Dir::read_from(&dir.fd).map_err(io)? {
        let name = std::ffi::OsString::from_vec(entry.map_err(io)?.file_name().to_bytes().to_vec());
        // Skip hidden names: emacs leaves a dangling `.#x.toml` symlink while editing.
        if !name.as_encoded_bytes().starts_with(b".")
            && Path::new(&name)
                .extension()
                .is_some_and(|ext| ext == "toml")
        {
            names.push(name);
        }
    }
    names.sort();
    let mut files = Vec::new();
    for name in names {
        let shown = dir.shown.join(&name);
        let entry = check_trusted(root, dir.at(), Path::new(&name), &shown, our_uid)?;
        if entry.kind == FileType::RegularFile {
            files.push(entry);
        }
    }
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
/// In a directory, files whose name starts with `.` are ignored.
///
/// An `exec` program is walked from `/` like the configuration and must pass
/// the same ownership and mode check; see [`ConfigError::UntrustedExec`] for
/// what that check does not cover.
///
/// Fails rather than degrading: an unreadable path, an empty directory, a
/// malformed file, an untrusted or missing `exec` program, or an
/// unresolvable user/group all return an error, and
/// `rsb_hub` refuses to start on any of them. There is no partial policy —
/// a policy that silently dropped the rule it could not parse would be a
/// policy that fails open.
pub fn load(path: &Path, resolver: &dyn NameResolver) -> Result<Config, ConfigError> {
    let io = |source| ConfigError::Io {
        path: path.to_owned(),
        source,
    };
    let root = super::trust::open_root(Path::new("/")).map_err(io)?;
    let abs = std::path::absolute(path).map_err(io)?;
    let rel = abs.strip_prefix("/").unwrap_or(&abs);
    let at = At {
        fd: root.as_fd(),
        shown: Path::new("/"),
    };
    load_at(at, rel, path, resolver)
}

/// `load` below a directory we hold; tests pass their own so nothing above it is looked at.
fn load_at(
    root: At<'_>,
    rel: &Path,
    given: &Path,
    resolver: &dyn NameResolver,
) -> Result<Config, ConfigError> {
    let our_uid = rustix::process::getuid().as_raw();
    let top = check_trusted(root, root, rel, given, our_uid)?;

    let files = match top.kind {
        FileType::Directory => {
            let files = policy_files(root, &top, our_uid)?;
            if files.is_empty() {
                return Err(ConfigError::NoConfigFiles(given.to_owned()));
            }
            files
        }
        FileType::RegularFile => vec![top],
        _ => {
            return Err(ConfigError::Io {
                path: top.shown,
                source: std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "not a regular file or a directory",
                ),
            })
        }
    };

    let mut config = Config::default();
    let mut global_origin: Option<PathBuf> = None;
    let mut service_origin: BTreeMap<String, PathBuf> = BTreeMap::new();

    for entry in files {
        let file = entry.shown.clone();
        let text = read_trusted(entry)?;
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
            if let Some(Activation::Exec(argv)) = &declaration.activation {
                check_exec(root, &file, &name, &argv[0], our_uid)?;
            }
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

    /// Fixed name service, so tests never depend on the build machine's accounts.
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

    /// A configuration directory `cfg` under a held root, loaded through `load_at`.
    struct ConfigDir {
        root: std::os::fd::OwnedFd,
        base: PathBuf,
    }

    fn chmod(path: &Path, mode: u32) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    impl ConfigDir {
        /// Explicit modes: under umask 0002 the defaults are group-writable and fail the check.
        fn new(tag: &str, files: &[(&str, &str)]) -> Self {
            let base = std::env::temp_dir().join(format!("rsb-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            std::fs::create_dir_all(&base).unwrap();
            chmod(&base, 0o755);
            let root = crate::config::trust::open_root(&base).unwrap();
            let this = ConfigDir { root, base };
            this.dir("cfg");
            for (name, text) in files {
                this.file(&format!("cfg/{name}"), text);
            }
            this
        }

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

        fn cfg(&self) -> PathBuf {
            self.base.join("cfg")
        }

        fn load(&self) -> Result<Config, ConfigError> {
            let at = At {
                fd: self.root.as_fd(),
                shown: &self.base,
            };
            load_at(at, Path::new("cfg"), &self.cfg(), &FakeResolver)
        }
    }

    impl Drop for ConfigDir {
        fn drop(&mut self) {
            chmod(&self.base, 0o755);
            let _ = std::fs::remove_dir_all(&self.base);
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

    /// File-name order decides precedence and the first match wins, so `10-` beats `99-`.
    #[test]
    fn directory_load_orders_by_file_name() {
        let dir = ConfigDir::new(
            "policy",
            &[
                (
                    "99-catchall.toml",
                    "[[rule]]\nname = \"*\"\nadd = \"any\"\nfind = \"any\"\n",
                ),
                (
                    "10-specific.toml",
                    "[global]\nlist = \"any\"\n\n[[rule]]\nname = \"locked.*\"\nadd = \"none\"\nfind = \"none\"\n",
                ),
                // Not a .toml — must be ignored, not parsed.
                ("README", "not toml at all {{{"),
            ],
        );

        let policy = dir.load().expect("directory must load").policy;
        let anyone = subject(1234, &[]);
        assert!(!policy.check(Permission::Add, "locked.foo", &anyone));
        assert!(policy.check(Permission::Add, "open.foo", &anyone));
        assert!(policy.check(Permission::List, "", &anyone));
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
        // Not startable, still declared: the client can tell "not installed" from "not started".
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

        let no_unit = parse_services("[[service]]\nname = \"a/b\"\nstart = { systemd = \"\" }\n")
            .unwrap_err();
        assert!(
            matches!(no_unit, ConfigError::EmptyUnit { .. }),
            "{no_unit:?}"
        );
    }

    #[test]
    fn an_undialable_connection_port_is_rejected() {
        for port in ["0", "-1", "65536", "70000"] {
            let text = format!(
                "[[service]]\nname = \"a/b\"\nconnection = {{ ip = \"127.0.0.1\", port = {port} }}\n"
            );
            let err = parse_services(&text).unwrap_err();
            assert!(matches!(err, ConfigError::Toml { .. }), "{port}: {err:?}");
        }
        let top =
            "[[service]]\nname = \"a/b\"\nconnection = { ip = \"127.0.0.1\", port = 65535 }\n";
        assert_eq!(
            parse_services(top)
                .unwrap()
                .connection_info("a/b")
                .map(|c| c.port),
            Some(65535)
        );
    }

    /// A relative program would be looked up through rsb_hub's `PATH` or cwd, as root.
    #[test]
    fn a_relative_exec_program_is_rejected() {
        for prog in ["exampled", "./exampled", "bin/exampled"] {
            let text = format!("[[service]]\nname = \"a/b\"\nstart = {{ exec = [\"{prog}\"] }}\n");
            let err = parse_services(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::RelativeExec { ref program, .. } if program == prog),
                "{err:?}"
            );
        }
    }

    /// The program runs with rsb_hub's privileges, so it gets the configuration's own check.
    #[test]
    fn an_exec_program_others_can_replace_is_refused() {
        let dir = ConfigDir::new(
            "cfg-exec",
            &[(
                "10.toml",
                "[[service]]\nname = \"a/b\"\nstart = { exec = [\"/bin/svc\", \"-v\"] }\n",
            )],
        );
        let bin = dir.dir("bin");
        let svc = dir.file("bin/svc", "#!/bin/sh\n");
        chmod(&svc, 0o755);
        assert!(dir.load().is_ok());

        chmod(&bin, 0o775);
        match dir.load().unwrap_err() {
            ConfigError::UntrustedExec {
                offender, problem, ..
            } => {
                assert_eq!(offender, bin);
                assert_eq!(problem, crate::config::TrustProblem::GroupWritable);
            }
            other => panic!("{other:?}"),
        }

        chmod(&bin, 0o755);
        chmod(&svc, 0o777);
        match dir.load().unwrap_err() {
            ConfigError::UntrustedExec {
                offender, problem, ..
            } => {
                assert_eq!(offender, svc);
                assert_eq!(problem, crate::config::TrustProblem::WorldWritable);
            }
            other => panic!("{other:?}"),
        }
    }

    /// Exec needs `x` only, so a mode-0111 program loads; root reads it anyway, so skip as root.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn an_execute_only_exec_program_is_accepted() {
        if rustix::process::geteuid().is_root() {
            eprintln!("skipped: root reads a mode-0111 file, so nothing is tested");
            return;
        }
        let dir = ConfigDir::new(
            "cfg-exec-xonly",
            &[(
                "10.toml",
                "[[service]]\nname = \"a/b\"\nstart = { exec = [\"/bin/svc\"] }\n",
            )],
        );
        dir.dir("bin");
        let svc = dir.file("bin/svc", "#!/bin/sh\n");
        chmod(&svc, 0o111);
        if let Err(err) = dir.load() {
            panic!("{err:?}");
        }
    }

    /// A missing program cannot be checked; its directory may let anyone create it later.
    #[test]
    fn a_missing_exec_program_is_refused() {
        let dir = ConfigDir::new(
            "cfg-exec-missing",
            &[(
                "10.toml",
                "[[service]]\nname = \"a/b\"\nstart = { exec = [\"/bin/absent\"] }\n",
            )],
        );
        dir.dir("bin");
        let err = dir.load().unwrap_err();
        assert!(
            matches!(&err, ConfigError::ExecIo { source, .. }
                if source.kind() == std::io::ErrorKind::NotFound),
            "{err:?}"
        );
    }

    /// No implicit `default` instance: `getDeclaredInstances` and `isDeclared` must agree.
    #[test]
    fn a_service_name_without_both_halves_is_rejected() {
        for name in ["com.example.IFoo", "com.example.IFoo/", "/default", "/"] {
            let text = format!("[[service]]\nname = \"{name}\"\n");
            let err = parse_services(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::BadServiceName { .. }),
                "{name}: {err:?}"
            );
        }
    }

    /// A name `addService` refuses could never be registered, so a client would wait forever.
    #[test]
    fn a_service_name_add_service_would_refuse_is_rejected() {
        let long = format!("a.IFoo/{}", "x".repeat(121));
        for name in [
            "a.IFoo/café",
            "a.IFoo/de fault",
            "a.IFoo/x@y",
            long.as_str(),
        ] {
            let text = format!("[[service]]\nname = \"{name}\"\n");
            let err = parse_services(&text).unwrap_err();
            assert!(
                matches!(err, ConfigError::BadServiceName { .. }),
                "{name}: {err:?}"
            );
        }
        assert!(parse_services(&format!(
            "[[service]]\nname = \"a.IFoo/{}\"\n",
            "x".repeat(120)
        ))
        .is_ok());
    }

    #[test]
    fn unknown_service_keys_are_rejected() {
        let err = parse_services("[[service]]\nname = \"a/b\"\nstrat = \"x\"\n").unwrap_err();
        assert!(matches!(err, ConfigError::Toml { .. }), "{err:?}");
    }

    /// Two files declaring the same instance is ambiguity about what runs.
    #[test]
    fn duplicate_service_declaration_is_an_error() {
        let dir = ConfigDir::new(
            "cfg-dupsvc",
            &[
                (
                    "10-a.toml",
                    "[[service]]\nname = \"a/b\"\nstart = { systemd = \"one.service\" }\n",
                ),
                (
                    "20-b.toml",
                    "[[service]]\nname = \"a/b\"\nstart = { systemd = \"two.service\" }\n",
                ),
            ],
        );
        let err = dir.load().unwrap_err();
        assert!(
            matches!(err, ConfigError::DuplicateService { ref name, .. } if name == "a/b"),
            "{err:?}"
        );
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
        let dir = ConfigDir::new("policy-empty", &[]);
        assert!(matches!(dir.load(), Err(ConfigError::NoConfigFiles(_))));
    }

    /// The configuration decides what runs, so whoever can rewrite it decides what runs.
    #[test]
    fn a_world_writable_directory_is_refused() {
        let dir = ConfigDir::new(
            "cfg-perm",
            &[("10.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n")],
        );

        // Sane to begin with.
        assert!(dir.load().is_ok());

        chmod(&dir.cfg(), 0o777);
        let err = dir.load().unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err:?}");

        // And a sane directory holding a writable file is refused too.
        chmod(&dir.cfg(), 0o755);
        chmod(&dir.cfg().join("10.toml"), 0o666);
        let err = dir.load().unwrap_err();
        assert!(matches!(err, ConfigError::Untrusted { .. }), "{err:?}");
    }

    #[test]
    fn duplicate_global_is_an_error() {
        let dir = ConfigDir::new(
            "policy-dup",
            &[
                ("10-a.toml", "[global]\nlist = \"any\"\n"),
                ("20-b.toml", "[global]\nlist = \"none\"\n"),
            ],
        );
        assert!(matches!(
            dir.load(),
            Err(ConfigError::DuplicateGlobal { .. })
        ));
    }

    /// The root fd is where the walk starts, so the temp directory's own mode never matters.
    #[test]
    fn load_at_does_not_look_above_the_root() {
        let outer = ConfigDir::new("above-root", &[]);
        let inner_base = outer.dir("root");
        let inner = ConfigDir {
            root: crate::config::trust::open_root(&inner_base).unwrap(),
            base: inner_base,
        };
        inner.dir("cfg");
        inner.file("cfg/10.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n");
        chmod(&outer.base, 0o777);

        let policy = inner
            .load()
            .expect("nothing above the root is checked")
            .policy;
        assert!(policy.check(Permission::Find, "x", &subject(1, &[])));
    }

    /// `--config /`, or a link to `.`, ends the walk on the held `O_PATH` root; it must still list.
    #[test]
    fn the_root_itself_loads_as_a_directory() {
        let dir = ConfigDir::new("cfg-root", &[]);
        dir.file("10.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n");
        std::os::unix::fs::symlink(".", dir.base.join("here")).unwrap();
        let at = At {
            fd: dir.root.as_fd(),
            shown: &dir.base,
        };
        for rel in ["", "here"] {
            let config = load_at(at, Path::new(rel), &dir.base, &FakeResolver)
                .unwrap_or_else(|e| panic!("{rel:?}: {e}"));
            assert!(config.policy.check(Permission::Find, "x", &subject(1, &[])));
        }
    }

    /// A symlinked entry is read, and the directory it points into is checked.
    #[test]
    fn a_symlinked_entry_is_followed_and_its_directory_checked() {
        let dir = ConfigDir::new("cfg-symlink", &[]);
        let shared = dir.dir("shared");
        dir.file("shared/10.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n");
        std::os::unix::fs::symlink("../shared/10.toml", dir.cfg().join("10.toml")).unwrap();

        let policy = dir.load().expect("a symlinked entry loads").policy;
        assert!(policy.check(Permission::Find, "x", &subject(1, &[])));

        chmod(&shared, 0o775);
        match dir.load().unwrap_err() {
            ConfigError::Untrusted { path, .. } => assert_eq!(path, shared),
            other => panic!("{other:?}"),
        }
    }

    /// Without `O_NONBLOCK` the open of the FIFO would wait for a writer; this test then hangs.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    #[test]
    fn a_fifo_named_toml_is_skipped_without_blocking() {
        use rustix::fs::{mknodat, Mode};
        let dir = ConfigDir::new(
            "cfg-fifo",
            &[("20.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n")],
        );
        mknodat(
            &dir.root,
            "cfg/10.toml",
            FileType::Fifo,
            Mode::from_raw_mode(0o644),
            0,
        )
        .unwrap();

        let policy = dir.load().expect("the FIFO is skipped").policy;
        assert_eq!(policy.rules.len(), 1);
    }

    #[test]
    fn hidden_toml_names_are_skipped() {
        let dir = ConfigDir::new(
            "cfg-hidden",
            &[
                ("10-a.toml", "[[rule]]\nname = \"*\"\nfind = \"any\"\n"),
                (".10-a.toml", "[[rule]]\nname = \"x\"\nfind = \"any\"\n"),
            ],
        );
        std::os::unix::fs::symlink("user@host.1:1", dir.cfg().join(".#10-a.toml")).unwrap();

        let policy = dir.load().expect("hidden names are skipped").policy;
        assert_eq!(policy.rules.len(), 1);
    }

    fn checked(dir: &ConfigDir, rel: &str) -> Trusted {
        let at = At {
            fd: dir.root.as_fd(),
            shown: &dir.base,
        };
        let us = rustix::process::getuid().as_raw();
        check_trusted(at, at, Path::new(rel), &dir.base.join(rel), us).expect("must be trusted")
    }

    /// What `read_trusted` returns is the inode `check_trusted` held, whatever the path is now.
    #[test]
    fn a_file_swapped_after_the_check_is_not_what_is_read() {
        let dir = ConfigDir::new("cfg-swap-file", &[("hub.toml", "A"), ("other.toml", "B")]);
        let entry = checked(&dir, "cfg/hub.toml");
        std::fs::rename(dir.cfg().join("other.toml"), dir.cfg().join("hub.toml")).unwrap();
        assert_eq!(read_trusted(entry).unwrap(), "A");
    }

    /// policy_files lists the held directory, not whatever either path names after a swap.
    #[test]
    fn a_directory_swapped_after_the_check_does_not_change_the_file_set() {
        let dir = ConfigDir::new("cfg-swap-link", &[]);
        dir.dir("real1");
        dir.dir("real2");
        dir.file("real1/10.toml", "");
        dir.file("real2/99.toml", "");
        let link = dir.cfg().join("hub.d");
        std::os::unix::fs::symlink("../real1", &link).unwrap();
        let at = At {
            fd: dir.root.as_fd(),
            shown: &dir.base,
        };

        let top = checked(&dir, "cfg/hub.d");
        assert_eq!(top.shown, dir.base.join("real1"));
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("../real2", &link).unwrap();
        std::fs::rename(dir.base.join("real1"), dir.base.join("old")).unwrap();
        std::fs::rename(dir.base.join("real2"), dir.base.join("real1")).unwrap();
        let us = rustix::process::getuid().as_raw();
        let names: Vec<PathBuf> = policy_files(at, &top, us)
            .unwrap()
            .into_iter()
            .map(|t| t.shown)
            .collect();
        assert_eq!(names, [dir.base.join("real1/10.toml")]);
    }
}

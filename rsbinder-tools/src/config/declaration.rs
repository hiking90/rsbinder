// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Service declarations — what `rsb_hub` knows about a service before
//! anyone registers it.
//!
//! AOSP answers `isDeclared` / `getDeclaredInstances` / `getConnectionInfo`
//! from VINTF manifests, an Android build artifact with no equivalent on a
//! plain Linux host. A declaration file is that equivalent: the operator
//! states which instances are expected to exist, so a client can tell
//! "not installed" from "not started yet" — which is the question
//! `isDeclared` exists to answer, and the one a lazy service depends on.

use std::collections::BTreeMap;

/// An AIDL instance name, split the way AOSP's `NameUtil.h` splits it:
/// `pack.age.IFoo/instance`.
///
/// The split is what `getDeclaredInstances` needs — it is asked for an
/// interface and answers with the instance halves.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct InstanceName {
    /// The interface half, before the `/`.
    pub interface: String,
    /// The instance half, after the `/`.
    pub instance: String,
}

impl InstanceName {
    /// Split `pack.age.IFoo/instance`.
    ///
    /// A name with no `/` is an interface with the implicit instance
    /// `"default"`, matching how AOSP's `IServiceManager` treats a bare
    /// interface name.
    pub fn parse(name: &str) -> InstanceName {
        match name.split_once('/') {
            Some((interface, instance)) => InstanceName {
                interface: interface.to_owned(),
                instance: instance.to_owned(),
            },
            None => InstanceName {
                interface: name.to_owned(),
                instance: "default".to_owned(),
            },
        }
    }
}

/// How to bring a declared service up on demand.
///
/// AOSP's `tryStartService` sets the `ctl.interface_start` property and
/// lets init do the work. On Linux the equivalent is the service manager,
/// so a declaration names either a systemd unit or a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Activation {
    /// `systemctl --no-block start <unit>`.
    Systemd(String),
    /// Spawn this command directly. Argv, never a shell string: a shell
    /// would make quoting in the config file part of the trust boundary.
    Exec(Vec<String>),
}

/// The `ConnectionInfo` an accessor-style client asks for. AOSP reads it
/// from a VINTF `<ip>`/`<port>` pair.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectionInfo {
    /// Address to dial.
    pub ip: String,
    /// Port to dial.
    pub port: i32,
}

/// One declared service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Declaration {
    /// The instance this declaration is for.
    pub name: InstanceName,
    /// How to start it on demand, if it can be started.
    pub activation: Option<Activation>,
    /// Connection info to report, if any.
    pub connection: Option<ConnectionInfo>,
}

/// Every declaration, indexed by full instance name.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Declarations {
    by_name: BTreeMap<String, Declaration>,
}

impl Declarations {
    /// Build from an iterator of `(full name, declaration)`.
    ///
    /// A repeated name is an error rather than a last-one-wins: two files
    /// declaring the same instance differently is ambiguity in a file that
    /// decides what runs.
    pub fn insert(&mut self, full_name: String, declaration: Declaration) -> Option<Declaration> {
        self.by_name.insert(full_name, declaration)
    }

    /// Is this instance declared? Answers `isDeclared`.
    pub fn is_declared(&self, name: &str) -> bool {
        self.by_name.contains_key(name)
    }

    /// The instance halves declared for `interface`, sorted. Answers
    /// `getDeclaredInstances`: given `pack.age.IFoo`, a declaration of
    /// `pack.age.IFoo/foo` contributes `"foo"`.
    pub fn instances_of(&self, interface: &str) -> Vec<String> {
        self.by_name
            .values()
            .filter(|d| d.name.interface == interface)
            .map(|d| d.name.instance.clone())
            .collect()
    }

    /// Connection info for an instance, if it declared any.
    pub fn connection_info(&self, name: &str) -> Option<&ConnectionInfo> {
        self.by_name.get(name).and_then(|d| d.connection.as_ref())
    }

    /// How to start an instance on demand, if it can be started.
    pub fn activation(&self, name: &str) -> Option<&Activation> {
        self.by_name.get(name).and_then(|d| d.activation.as_ref())
    }

    /// Number of declarations, for startup logging.
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether anything is declared at all.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decl(name: &str) -> (String, Declaration) {
        (
            name.to_owned(),
            Declaration {
                name: InstanceName::parse(name),
                activation: None,
                connection: None,
            },
        )
    }

    #[test]
    fn instance_names_split_on_the_slash() {
        let n = InstanceName::parse("com.example.IFoo/default");
        assert_eq!(n.interface, "com.example.IFoo");
        assert_eq!(n.instance, "default");
    }

    /// A bare interface name means its `default` instance, as AOSP treats it.
    #[test]
    fn a_bare_name_is_the_default_instance() {
        let n = InstanceName::parse("com.example.IFoo");
        assert_eq!(n.interface, "com.example.IFoo");
        assert_eq!(n.instance, "default");
    }

    /// Only the first `/` splits — an instance may contain one.
    #[test]
    fn only_the_first_slash_splits() {
        let n = InstanceName::parse("com.example.IFoo/a/b");
        assert_eq!(n.interface, "com.example.IFoo");
        assert_eq!(n.instance, "a/b");
    }

    #[test]
    fn declared_instances_are_grouped_by_interface() {
        let mut d = Declarations::default();
        for name in [
            "com.example.IFoo/default",
            "com.example.IFoo/secondary",
            "com.example.IBar/default",
        ] {
            let (k, v) = decl(name);
            d.insert(k, v);
        }
        assert_eq!(
            d.instances_of("com.example.IFoo"),
            vec!["default".to_owned(), "secondary".to_owned()]
        );
        assert_eq!(d.instances_of("com.example.IBar"), vec!["default"]);
        assert!(d.instances_of("com.example.INope").is_empty());

        assert!(d.is_declared("com.example.IFoo/default"));
        // The interface alone is not an instance.
        assert!(!d.is_declared("com.example.IFoo"));
    }

    /// A prefix must not be mistaken for an interface match.
    #[test]
    fn interface_match_is_exact() {
        let mut d = Declarations::default();
        let (k, v) = decl("com.example.IFooBar/default");
        d.insert(k, v);
        assert!(d.instances_of("com.example.IFoo").is_empty());
    }

    #[test]
    fn missing_metadata_reports_none() {
        let mut d = Declarations::default();
        let (k, v) = decl("com.example.IFoo/default");
        d.insert(k, v);
        assert!(d.connection_info("com.example.IFoo/default").is_none());
        assert!(d.activation("com.example.IFoo/default").is_none());
        assert!(d.activation("com.example.INope/default").is_none());
    }
}

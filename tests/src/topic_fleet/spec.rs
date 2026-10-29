// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The roles of a `topic_node`, one per argument pair: `--<role> key=value,key,...`.
//!
//! ```text
//! --pub       topic=NAME [payload=bytes|record] [hz=10] [size=32] [ring=BYTES] [subs=N]
//!             [pool] [bp=MS] [window=N] [via=sm|REGISTRY] [prepub=K]
//! --sub       topic=NAME [payload=bytes|record] [api=plain|named] [recv=recv|latest|sample|async]
//!             [wake=futex|fd] [start=latest|last|oldest] [trust] [group=G] [via=sm|REGISTRY]
//! --fuse      out=NAME frames=NAME readings=NAME [via=sm]
//! --relay     topic=NAME service=NAME
//! --report    service=NAME [hz=2]
//! --registry  service=NAME
//! ```
//!
//! `via` names how a topic is found: the service manager (`sm`, the default) or the topic
//! registry registered with it under that name. Subscriptions of one `group` share a thread
//! that waits on them with `wait_any`, which takes `wake=fd`. `prepub=K` makes the topic,
//! publishes `K` records, registers it with the service manager, and publishes nothing more
//! until `resume` (X1: a connection then has records to start from).

use rsbinder_topic::{Start, Wake};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PayloadKind {
    Bytes,
    Record,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Via {
    ServiceManager,
    /// The service name of a topic registry.
    Registry(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Api {
    Plain,
    Named,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recv {
    Recv,
    Latest,
    Sample,
    Async,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PubSpec {
    pub topic: String,
    pub payload: PayloadKind,
    pub hz: u32,
    /// Bytes of a `Bytes` record, and of a pool slot.
    pub size: usize,
    pub ring_bytes: Option<usize>,
    pub max_subscribers: Option<usize>,
    pub pool: bool,
    /// The backpressure timeout in milliseconds.
    pub backpressure_ms: Option<u64>,
    /// The backpressure window: the most records a subscriber may be behind.
    pub window: Option<usize>,
    pub via: Via,
    /// Records published before the topic is registered, after which it waits for `resume`.
    pub prepublish: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SubSpec {
    pub topic: String,
    pub payload: PayloadKind,
    pub api: Api,
    pub recv: Recv,
    pub wake: Wake,
    pub start: Start,
    pub trust: bool,
    pub group: Option<String>,
    pub via: Via,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Role {
    Pub(PubSpec),
    Sub(SubSpec),
    /// Subscribe to `frames` (T1) and `readings` (T2), publish a record on `out` (T4) per frame.
    Fuse {
        out: String,
        frames: String,
        readings: String,
    },
    /// Serve `IFleetStatus` under `service` and publish each report on `topic` (T5).
    Relay {
        topic: String,
        service: String,
    },
    /// Report to the relay under `service` `hz` times a second.
    Report {
        service: String,
        hz: u32,
    },
    /// Host a topic registry under `service`.
    Registry {
        service: String,
    },
}

/// The `key=value` and bare `key` items of a spec, each taken once.
struct Items<'a>(Vec<(&'a str, Option<&'a str>)>);

impl<'a> Items<'a> {
    fn new(spec: &'a str) -> Self {
        Self(
            spec.split(',')
                .filter(|item| !item.is_empty())
                .map(|item| match item.split_once('=') {
                    Some((key, value)) => (key, Some(value)),
                    None => (item, None),
                })
                .collect(),
        )
    }

    fn take(&mut self, key: &str) -> Option<Option<&'a str>> {
        let at = self.0.iter().position(|&(k, _)| k == key)?;
        Some(self.0.remove(at).1)
    }

    fn value(&mut self, key: &str) -> Result<Option<&'a str>, String> {
        match self.take(key) {
            None => Ok(None),
            Some(Some(value)) => Ok(Some(value)),
            Some(None) => Err(format!("{key} without a value")),
        }
    }

    fn required(&mut self, key: &str) -> Result<String, String> {
        self.value(key)?
            .map(str::to_string)
            .ok_or_else(|| format!("no {key}"))
    }

    fn number<N: std::str::FromStr>(&mut self, key: &str, default: N) -> Result<N, String> {
        match self.value(key)? {
            None => Ok(default),
            Some(value) => value
                .parse()
                .map_err(|_| format!("{key}={value} is not a number")),
        }
    }

    fn flag(&mut self, key: &str) -> Result<bool, String> {
        match self.take(key) {
            None => Ok(false),
            Some(None) => Ok(true),
            Some(Some(value)) => Err(format!("{key}={value}: {key} takes no value")),
        }
    }

    fn choice<T: Copy>(&mut self, key: &str, choices: &[(&str, T)]) -> Result<T, String> {
        let value = self.value(key)?;
        let value = value.unwrap_or(choices[0].0);
        choices
            .iter()
            .find(|(name, _)| *name == value)
            .map(|&(_, choice)| choice)
            .ok_or_else(|| format!("{key}={value}"))
    }

    fn via(&mut self) -> Result<Via, String> {
        Ok(match self.value("via")? {
            None | Some("sm") => Via::ServiceManager,
            Some(registry) => Via::Registry(registry.into()),
        })
    }

    fn done(self) -> Result<(), String> {
        match self.0.first() {
            None => Ok(()),
            Some((key, _)) => Err(format!("unknown {key}")),
        }
    }
}

const PAYLOADS: &[(&str, PayloadKind)] = &[
    ("bytes", PayloadKind::Bytes),
    ("record", PayloadKind::Record),
];

impl Role {
    /// The role of `--<flag> <spec>`.
    pub fn parse(flag: &str, spec: &str) -> Result<Self, String> {
        let mut items = Items::new(spec);
        let role = match flag {
            "--pub" => Role::Pub(PubSpec {
                topic: items.required("topic")?,
                payload: items.choice("payload", PAYLOADS)?,
                hz: items.number("hz", 10)?,
                size: items.number("size", super::record::HEADER)?,
                ring_bytes: items
                    .value("ring")?
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| "ring")?,
                max_subscribers: items
                    .value("subs")?
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| "subs")?,
                pool: items.flag("pool")?,
                backpressure_ms: items
                    .value("bp")?
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| "bp")?,
                window: items
                    .value("window")?
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| "window")?,
                via: items.via()?,
                prepublish: items
                    .value("prepub")?
                    .map(str::parse)
                    .transpose()
                    .map_err(|_| "prepub")?,
            }),
            "--sub" => Role::Sub(SubSpec {
                topic: items.required("topic")?,
                payload: items.choice("payload", PAYLOADS)?,
                api: items.choice("api", &[("plain", Api::Plain), ("named", Api::Named)])?,
                recv: items.choice(
                    "recv",
                    &[
                        ("recv", Recv::Recv),
                        ("latest", Recv::Latest),
                        ("sample", Recv::Sample),
                        ("async", Recv::Async),
                    ],
                )?,
                wake: items.choice("wake", &[("futex", Wake::Futex), ("fd", Wake::Fd)])?,
                start: items.choice(
                    "start",
                    &[
                        ("latest", Start::Latest),
                        ("last", Start::Last),
                        ("oldest", Start::Oldest),
                    ],
                )?,
                trust: items.flag("trust")?,
                group: items.value("group")?.map(str::to_string),
                via: items.via()?,
            }),
            "--fuse" => Role::Fuse {
                out: items.required("out")?,
                frames: items.required("frames")?,
                readings: items.required("readings")?,
            },
            "--relay" => Role::Relay {
                topic: items.required("topic")?,
                service: items.required("service")?,
            },
            "--report" => Role::Report {
                service: items.required("service")?,
                hz: items.number("hz", 2)?,
            },
            "--registry" => Role::Registry {
                service: items.required("service")?,
            },
            _ => return Err(format!("unknown role {flag}")),
        };
        items.done()?;
        role.check()?;
        Ok(role)
    }

    /// The combinations the topic API does not have.
    fn check(&self) -> Result<(), String> {
        match self {
            Role::Pub(spec) if spec.hz == 0 => Err("hz=0".into()),
            Role::Pub(spec) if spec.pool && spec.payload != PayloadKind::Bytes => {
                Err("a pool lends slots of bytes".into())
            }
            Role::Pub(spec) if spec.prepublish.is_some() && spec.via != Via::ServiceManager => {
                Err("prepub registers with the service manager".into())
            }
            Role::Pub(spec)
                if spec.window.is_some_and(|window| window == 0)
                    || (spec.window.is_some() && spec.backpressure_ms.is_none()) =>
            {
                Err("a window is of backpressure, and at least 1".into())
            }
            Role::Report { hz: 0, .. } => Err("hz=0".into()),
            Role::Sub(spec) => {
                let plain_bytes = spec.api == Api::Plain && spec.payload == PayloadKind::Bytes;
                if (spec.recv == Recv::Sample || spec.trust) && !plain_bytes {
                    return Err(
                        "recv_sample and trust_publisher are a plain subscriber's of bytes".into(),
                    );
                }
                if spec.trust && spec.recv != Recv::Sample {
                    return Err("trust is for recv=sample".into());
                }
                if spec.group.is_some() && (spec.wake != Wake::Fd || spec.recv == Recv::Async) {
                    return Err("a group waits with wait_any: wake=fd, and no recv=async".into());
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    /// The topic this role publishes, if any.
    pub fn published(&self) -> Option<&str> {
        match self {
            Role::Pub(spec) => Some(&spec.topic),
            Role::Fuse { out, .. } => Some(out),
            Role::Relay { topic, .. } => Some(topic),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_fill_what_a_spec_leaves_out() {
        assert_eq!(
            Role::parse("--sub", "topic=t"),
            Ok(Role::Sub(SubSpec {
                topic: "t".into(),
                payload: PayloadKind::Bytes,
                api: Api::Plain,
                recv: Recv::Recv,
                wake: Wake::Futex,
                start: Start::Latest,
                trust: false,
                group: None,
                via: Via::ServiceManager,
            }))
        );
        let Ok(Role::Pub(spec)) = Role::parse("--pub", "topic=t,hz=1000,ring=4096,bp=1000,via=reg")
        else {
            panic!()
        };
        assert_eq!(
            (
                spec.hz,
                spec.ring_bytes,
                spec.backpressure_ms,
                spec.size,
                spec.via
            ),
            (
                1000,
                Some(4096),
                Some(1000),
                32,
                Via::Registry("reg".into())
            )
        );
    }

    #[test]
    fn every_role_parses() {
        for (flag, spec) in [
            ("--pub", "topic=t,payload=bytes,size=1048576,pool,subs=32"),
            ("--sub", "topic=t,recv=sample,trust"),
            (
                "--sub",
                "topic=t,api=named,recv=latest,wake=fd,start=oldest,group=g",
            ),
            (
                "--sub",
                "topic=t,payload=record,recv=async,wake=fd,start=last",
            ),
            ("--pub", "topic=t,payload=record,hz=1000,prepub=50"),
            ("--fuse", "out=a,frames=b,readings=c"),
            ("--relay", "topic=t,service=s"),
            ("--report", "service=s,hz=5"),
            ("--registry", "service=s"),
        ] {
            assert!(
                Role::parse(flag, spec).is_ok(),
                "{flag} {spec}: {:?}",
                Role::parse(flag, spec)
            );
        }
    }

    #[test]
    fn a_spec_the_api_cannot_serve_is_refused() {
        for (flag, spec) in [
            ("--pub", "hz=1"),
            ("--pub", "topic=t,hz=0"),
            ("--pub", "topic=t,payload=record,pool"),
            ("--pub", "topic=t,color=red"),
            ("--pub", "topic=t,pool=1"),
            ("--pub", "topic=t,prepub=5,via=reg"),
            ("--pub", "topic=t,prepub=x"),
            ("--pub", "topic=t,window=8"),
            ("--pub", "topic=t,bp=1000,window=0"),
            ("--sub", "topic=t,api=named,recv=sample"),
            ("--sub", "topic=t,payload=record,recv=sample"),
            ("--sub", "topic=t,trust"),
            ("--sub", "topic=t,group=g"),
            ("--sub", "topic=t,wake=fd,recv=async,group=g"),
            ("--sub", "topic=t,start=first"),
            ("--relay", "topic=t"),
            ("--dance", "topic=t"),
        ] {
            assert!(Role::parse(flag, spec).is_err(), "{flag} {spec}");
        }
    }
}

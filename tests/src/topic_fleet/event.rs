// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! One line of a node's stdout: `<mono_ns> <node> <KIND> <field>...`.
//!
//! Fields are separated by one space; only the last field of `VIOLATION` and `FAULT`, free text,
//! may hold spaces. Later stages add kinds and never reorder the fields of one.

use std::fmt;

use rsbinder_topic::{Closed, Disconnected};

use super::record::Incarnation;

/// What a node reports, stamped with [`now_ns`](super::now_ns) when it happened.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    pub at_ns: u64,
    pub node: String,
    pub kind: Kind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The node made its role's objects; the controller injects nothing into it before this.
    Ready { pid: u32 },
    /// The first record of a connection arrived.
    Connected {
        topic: String,
        incarnation: Incarnation,
        first_seq: u64,
    },
    /// What a connection has received, from its first counted seq to its last one.
    ///
    /// `first_seq` is the first record's seq less the records lagged before it, so a connection
    /// that lost none has `received + lagged == last_seq - first_seq + 1`. Sent only once a
    /// record arrived.
    Count {
        topic: String,
        received: u64,
        lagged: u64,
        first_seq: u64,
        last_seq: u64,
    },
    /// A receive returned `Closed` or `Disconnected`; `reason` is [`closed_reason`] or
    /// [`disconnected_reason`].
    Ended { topic: String, reason: String },
    /// A publisher stopped publishing at `exit`; its last record was `last_seq`.
    Published {
        topic: String,
        incarnation: Incarnation,
        last_seq: u64,
    },
    /// A check of a connection failed.
    Violation { topic: String, text: String },
    /// The node cannot go on.
    Fault { text: String },
    /// At `stats`: a subscription's `stats()`, then what the node counted over what those cover
    /// (a `NamedSubscriber`'s every connection, a `Subscriber`'s current one), and for a
    /// `NamedSubscriber` whether it is connected (a plain one always is).
    SubStats {
        topic: String,
        received: u64,
        lost: u64,
        decode_errors: u64,
        connections: u64,
        node_received: u64,
        node_lagged: u64,
        connected: bool,
    },
    /// At `stats`: a publisher's `stats()`.
    PubStats {
        topic: String,
        published: u64,
        evicted: u64,
        subscribers: Vec<Subscribed>,
        /// Bytes of payload published, the last field of the line.
        bytes: u64,
    },
    /// At `stats`: a registry host's names with a topic and watches.
    RegStats {
        service: String,
        topics: usize,
        watchers: usize,
    },
    /// At `stats`: the reports a reporter sent.
    Reported { service: String, sent: u64 },
    /// At `stats`: the latest report a T5 subscription received from each reporter's run.
    ReportSeen {
        topic: String,
        reporter: String,
        incarnation: Incarnation,
        seq: u64,
    },
    /// The last line of a node's answer to `stats`.
    StatsDone,
    /// A role of a topic found through a registry could not reach it, and waits for `rebind`
    /// (X14: the registry's host died as the role started).
    Unbound { topic: String, text: String },
    /// A subscription was refused with `ITopic::ERROR_FULL`: retried at 1 to 30 s, so the time
    /// bounds of §5.4 do not hold for it, and the topology sized its topics to avoid it (§3.3).
    Full { topic: String },
}

/// One subscription in a [`Kind::PubStats`]: `pid:lost:evicted` in the line.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Subscribed {
    pub pid: i32,
    pub lost: u64,
    pub evicted: bool,
}

impl fmt::Display for Subscribed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.pid, self.lost, u8::from(self.evicted))
    }
}

impl std::str::FromStr for Subscribed {
    type Err = String;

    fn from_str(word: &str) -> Result<Self, String> {
        let [pid, lost, evicted] = split::<3>(word, ':')?;
        Ok(Self {
            pid: number(pid)?,
            lost: number(lost)?,
            evicted: flag(evicted)?,
        })
    }
}

fn flag(word: &str) -> Result<bool, String> {
    match word {
        "0" => Ok(false),
        "1" => Ok(true),
        _ => Err(format!("not 0 or 1: {word:?}")),
    }
}

/// The `ENDED` token of a [`Closed`]: its variant's name, without a status's message.
pub fn closed_reason(closed: &Closed) -> &'static str {
    match closed {
        Closed::End => "End",
        Closed::Status(_) => "Status",
        Closed::PublisherDied => "PublisherDied",
        Closed::Corrupted => "Corrupted",
        Closed::Evicted => "Evicted",
        _ => "Unknown",
    }
}

/// The `ENDED` token of a [`Disconnected`]: `Ended.<closed>`, `Rejected` or `Replaced`.
pub fn disconnected_reason(why: &Disconnected) -> String {
    match why {
        Disconnected::Ended(closed) => format!("Ended.{}", closed_reason(closed)),
        Disconnected::Rejected(_) => "Rejected".into(),
        Disconnected::Replaced => "Replaced".into(),
        _ => "Unknown".into(),
    }
}

impl Event {
    pub fn new(node: &str, kind: Kind) -> Self {
        Self {
            at_ns: super::now_ns(),
            node: node.into(),
            kind,
        }
    }

    /// The line, without its newline; a newline in free text becomes a space.
    pub fn line(&self) -> String {
        self.to_string()
    }

    pub fn parse(line: &str) -> Result<Self, String> {
        let mut head = line.splitn(4, ' ');
        let (Some(at), Some(node), Some(kind)) = (head.next(), head.next(), head.next()) else {
            return Err(format!("too few fields: {line:?}"));
        };
        let rest = head.next().unwrap_or("");
        let at_ns = number(at)?;
        let kind = match kind {
            "READY" => {
                let [pid] = words(rest)?;
                Kind::Ready { pid: number(pid)? }
            }
            "CONNECTED" => {
                let [topic, incarnation, first_seq] = words(rest)?;
                Kind::Connected {
                    topic: topic.into(),
                    incarnation: incarnation.parse()?,
                    first_seq: number(first_seq)?,
                }
            }
            "COUNT" => {
                let [topic, received, lagged, first_seq, last_seq] = words(rest)?;
                Kind::Count {
                    topic: topic.into(),
                    received: number(received)?,
                    lagged: number(lagged)?,
                    first_seq: number(first_seq)?,
                    last_seq: number(last_seq)?,
                }
            }
            "ENDED" => {
                let [topic, reason] = words(rest)?;
                Kind::Ended {
                    topic: topic.into(),
                    reason: reason.into(),
                }
            }
            "PUBLISHED" => {
                let [topic, incarnation, last_seq] = words(rest)?;
                Kind::Published {
                    topic: topic.into(),
                    incarnation: incarnation.parse()?,
                    last_seq: number(last_seq)?,
                }
            }
            "VIOLATION" => {
                let (topic, text) = rest
                    .split_once(' ')
                    .ok_or_else(|| format!("VIOLATION without text: {line:?}"))?;
                Kind::Violation {
                    topic: topic.into(),
                    text: text.into(),
                }
            }
            "FAULT" => Kind::Fault { text: rest.into() },
            "SSTATS" => {
                let [topic, received, lost, decode_errors, connections, node_received, node_lagged, connected] =
                    words(rest)?;
                Kind::SubStats {
                    topic: topic.into(),
                    received: number(received)?,
                    lost: number(lost)?,
                    decode_errors: number(decode_errors)?,
                    connections: number(connections)?,
                    node_received: number(node_received)?,
                    node_lagged: number(node_lagged)?,
                    connected: flag(connected)?,
                }
            }
            "PSTATS" => {
                let [topic, published, evicted, subscribers, bytes] = words(rest)?;
                let subscribers = match subscribers {
                    "-" => Vec::new(),
                    list => list.split(',').map(str::parse).collect::<Result<_, _>>()?,
                };
                Kind::PubStats {
                    topic: topic.into(),
                    published: number(published)?,
                    evicted: number(evicted)?,
                    subscribers,
                    bytes: number(bytes)?,
                }
            }
            "RSTATS" => {
                let [service, topics, watchers] = words(rest)?;
                Kind::RegStats {
                    service: service.into(),
                    topics: number(topics)?,
                    watchers: number(watchers)?,
                }
            }
            "REPORTED" => {
                let [service, sent] = words(rest)?;
                Kind::Reported {
                    service: service.into(),
                    sent: number(sent)?,
                }
            }
            "RSEEN" => {
                let [topic, reporter, incarnation, seq] = words(rest)?;
                Kind::ReportSeen {
                    topic: topic.into(),
                    reporter: reporter.into(),
                    incarnation: incarnation.parse()?,
                    seq: number(seq)?,
                }
            }
            "STATSDONE" if rest.is_empty() => Kind::StatsDone,
            "UNBOUND" => {
                let (topic, text) = rest
                    .split_once(' ')
                    .ok_or_else(|| format!("UNBOUND without text: {line:?}"))?;
                Kind::Unbound {
                    topic: topic.into(),
                    text: text.into(),
                }
            }
            "FULL" => {
                let [topic] = words(rest)?;
                Kind::Full {
                    topic: topic.into(),
                }
            }
            other => return Err(format!("unknown kind {other:?}: {line:?}")),
        };
        Ok(Self {
            at_ns,
            node: node.into(),
            kind,
        })
    }
}

impl fmt::Display for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {} ", self.at_ns, self.node)?;
        match &self.kind {
            Kind::Ready { pid } => write!(f, "READY {pid}"),
            Kind::Connected {
                topic,
                incarnation,
                first_seq,
            } => write!(f, "CONNECTED {topic} {incarnation} {first_seq}"),
            Kind::Count {
                topic,
                received,
                lagged,
                first_seq,
                last_seq,
            } => write!(
                f,
                "COUNT {topic} {received} {lagged} {first_seq} {last_seq}"
            ),
            Kind::Ended { topic, reason } => write!(f, "ENDED {topic} {reason}"),
            Kind::Published {
                topic,
                incarnation,
                last_seq,
            } => write!(f, "PUBLISHED {topic} {incarnation} {last_seq}"),
            Kind::Violation { topic, text } => write!(f, "VIOLATION {topic} {}", one_line(text)),
            Kind::Fault { text } => write!(f, "FAULT {}", one_line(text)),
            Kind::SubStats {
                topic,
                received,
                lost,
                decode_errors,
                connections,
                node_received,
                node_lagged,
                connected,
            } => write!(
                f,
                "SSTATS {topic} {received} {lost} {decode_errors} {connections} {node_received} \
                 {node_lagged} {}",
                u8::from(*connected)
            ),
            Kind::PubStats {
                topic,
                published,
                evicted,
                subscribers,
                bytes,
            } => {
                let list: Vec<String> = subscribers.iter().map(Subscribed::to_string).collect();
                let list = if list.is_empty() {
                    "-".to_string()
                } else {
                    list.join(",")
                };
                write!(f, "PSTATS {topic} {published} {evicted} {list} {bytes}")
            }
            Kind::RegStats {
                service,
                topics,
                watchers,
            } => write!(f, "RSTATS {service} {topics} {watchers}"),
            Kind::Reported { service, sent } => write!(f, "REPORTED {service} {sent}"),
            Kind::ReportSeen {
                topic,
                reporter,
                incarnation,
                seq,
            } => write!(f, "RSEEN {topic} {reporter} {incarnation} {seq}"),
            Kind::StatsDone => f.write_str("STATSDONE"),
            Kind::Unbound { topic, text } => write!(f, "UNBOUND {topic} {}", one_line(text)),
            Kind::Full { topic } => write!(f, "FULL {topic}"),
        }
    }
}

fn one_line(text: &str) -> String {
    text.replace(['\n', '\r'], " ")
}

fn number<N: std::str::FromStr>(field: &str) -> Result<N, String> {
    field
        .parse()
        .map_err(|_| format!("not a number: {field:?}"))
}

/// Exactly `N` space-separated words.
fn words<const N: usize>(rest: &str) -> Result<[&str; N], String> {
    split(rest, ' ')
}

/// Exactly `N` parts of `text` between `separator`s.
fn split<const N: usize>(text: &str, separator: char) -> Result<[&str; N], String> {
    let parts: Vec<&str> = text.split(separator).collect();
    parts
        .try_into()
        .map_err(|parts: Vec<&str>| format!("{} fields, not {N}: {text:?}", parts.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn every_kind() -> Vec<Kind> {
        let topic = "rsb.fleet.f0.frames.42".to_string();
        vec![
            Kind::Ready { pid: 4242 },
            Kind::Connected {
                topic: topic.clone(),
                incarnation: Incarnation { pid: 7, start: 2 },
                first_seq: 19,
            },
            Kind::Count {
                topic: topic.clone(),
                received: 300,
                lagged: 4,
                first_seq: 19,
                last_seq: 322,
            },
            Kind::Ended {
                topic: topic.clone(),
                reason: disconnected_reason(&Disconnected::Ended(Closed::PublisherDied)),
            },
            Kind::Published {
                topic: topic.clone(),
                incarnation: Incarnation { pid: 7, start: 2 },
                last_seq: 322,
            },
            Kind::Violation {
                topic: topic.clone(),
                text: "seq 9 after 10".into(),
            },
            Kind::Fault {
                text: "subscribe: NameNotFound".into(),
            },
            Kind::SubStats {
                topic: topic.clone(),
                received: 10,
                lost: 2,
                decode_errors: 0,
                connections: 3,
                node_received: 10,
                node_lagged: 2,
                connected: true,
            },
            Kind::PubStats {
                topic: topic.clone(),
                published: 99,
                evicted: 1,
                subscribers: vec![
                    Subscribed {
                        pid: 12,
                        lost: 0,
                        evicted: false,
                    },
                    Subscribed {
                        pid: 13,
                        lost: 5,
                        evicted: true,
                    },
                ],
                bytes: 3168,
            },
            Kind::PubStats {
                topic: topic.clone(),
                published: 0,
                evicted: 0,
                subscribers: Vec::new(),
                bytes: 0,
            },
            Kind::Full {
                topic: topic.clone(),
            },
            Kind::RegStats {
                service: "rsbinder.topic.registry.fleet.1".into(),
                topics: 1,
                watchers: 4,
            },
            Kind::Reported {
                service: "rsb.fleet.t.status.1".into(),
                sent: 17,
            },
            Kind::ReportSeen {
                topic,
                reporter: "prod-2".into(),
                incarnation: Incarnation { pid: 8, start: 1 },
                seq: 16,
            },
            Kind::StatsDone,
            Kind::Unbound {
                topic: "rsb.fleet.t.named.reg.1".into(),
                text: "advertise: DeadObject".into(),
            },
        ]
    }

    #[test]
    fn every_kind_survives_its_line() {
        for kind in every_kind() {
            let event = Event {
                at_ns: 123_456_789,
                node: "sub-1".into(),
                kind,
            };
            assert_eq!(
                Event::parse(&event.line()),
                Ok(event.clone()),
                "{}",
                event.line()
            );
        }
    }

    #[test]
    fn free_text_stays_on_one_line() {
        let event = Event {
            at_ns: 1,
            node: "pub".into(),
            kind: Kind::Fault {
                text: "first\nsecond".into(),
            },
        };
        assert!(!event.line().contains('\n'));
        assert_eq!(
            Event::parse(&event.line()).map(|event| event.kind),
            Ok(Kind::Fault {
                text: "first second".into()
            })
        );
    }

    #[test]
    fn a_malformed_line_is_refused() {
        for line in [
            "",
            "1 node",
            "x node READY 1",
            "1 node READY",
            "1 node READY 1 2",
            "1 node COUNT t 1 2 3",
            "1 node CONNECTED t 7 0",
            "1 node VIOLATION t",
            "1 node HELLO",
        ] {
            assert!(Event::parse(line).is_err(), "{line:?}");
        }
    }
}

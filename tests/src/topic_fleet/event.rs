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
    let words: Vec<&str> = rest.split(' ').collect();
    words
        .try_into()
        .map_err(|words: Vec<&str>| format!("{} fields, not {N}: {rest:?}", words.len()))
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
                topic,
                text: "seq 9 after 10".into(),
            },
            Kind::Fault {
                text: "subscribe: NameNotFound".into(),
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

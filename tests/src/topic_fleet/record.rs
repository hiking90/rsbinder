// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The record a fleet publisher writes: who wrote it and its place in that writer's sequence.

use std::fmt;
use std::str::FromStr;

use rsbinder_topic::Bytes;

/// One run of a producer: its pid and the controller's start count of that node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Incarnation {
    pub pid: u32,
    pub start: u32,
}

/// `pid` (u32), `start` (u32), `seq` (u64), little-endian.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub incarnation: Incarnation,
    pub seq: u64,
}

const LEN: usize = 16;

impl Stamp {
    pub fn encode(&self) -> Bytes {
        let mut bytes = Vec::with_capacity(LEN);
        bytes.extend_from_slice(&self.incarnation.pid.to_le_bytes());
        bytes.extend_from_slice(&self.incarnation.start.to_le_bytes());
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        Bytes(bytes)
    }

    /// `None` for a record of another length.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; LEN] = bytes.try_into().ok()?;
        let (pid, rest) = bytes.split_at(4);
        let (start, seq) = rest.split_at(4);
        Some(Self {
            incarnation: Incarnation {
                pid: u32::from_le_bytes(pid.try_into().ok()?),
                start: u32::from_le_bytes(start.try_into().ok()?),
            },
            seq: u64::from_le_bytes(seq.try_into().ok()?),
        })
    }
}

/// `<pid>.<start>`, one word of an event line.
impl fmt::Display for Incarnation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.pid, self.start)
    }
}

impl FromStr for Incarnation {
    type Err = String;

    fn from_str(word: &str) -> Result<Self, String> {
        let parsed = word
            .split_once('.')
            .and_then(|(pid, start)| Some((pid.parse().ok()?, start.parse().ok()?)));
        let (pid, start) = parsed.ok_or_else(|| format!("not <pid>.<start>: {word:?}"))?;
        Ok(Self { pid, start })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stamp_survives_its_bytes() {
        let stamp = Stamp {
            incarnation: Incarnation {
                pid: 0x0102_0304,
                start: 7,
            },
            seq: u64::MAX - 1,
        };
        assert_eq!(Stamp::decode(&stamp.encode().0), Some(stamp));
        assert_eq!(Stamp::decode(&[0; LEN - 1]), None);
        assert_eq!(Stamp::decode(&[0; LEN + 1]), None);
    }
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The record a fleet publisher writes: which topic, who wrote it, its place in that writer's
//! sequence, and a checksum over all of it (plan 15 fleet test §5.1).
//!
//! ```text
//! 0   topic id  u32   fnv1a32 of the topic's name
//! 4   pid       u32   the writer's incarnation
//! 8   start     u32
//! 12  length    u32   of the whole record, a multiple of 8, at least 32
//! 16  seq       u64
//! 24  checksum  u64   FNV-1a over the 64-bit words of the record but this one
//! 32  body            words derived from seq
//! ```
//!
//! All little-endian. The checksum covers the body, so a subscriber that reads a record the
//! publisher was overwriting, or a `Trusted` subscriber's copy of a lent slot, fails it.

use std::fmt;
use std::str::FromStr;

use rsbinder_topic::Bytes;

/// One run of a producer: its pid and the controller's start count of that node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Incarnation {
    pub pid: u32,
    pub start: u32,
}

/// What a record says about itself.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub topic: u32,
    pub incarnation: Incarnation,
    pub seq: u64,
}

pub const HEADER: usize = 32;
const CHECKSUM: usize = 24;

/// The id a record of `topic` carries.
pub fn topic_id(topic: &str) -> u32 {
    topic.bytes().fold(0x811c_9dc5, |hash: u32, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    })
}

fn word(bytes: &[u8], at: usize) -> u64 {
    let mut word = [0; 8];
    word.copy_from_slice(&bytes[at..at + 8]);
    u64::from_le_bytes(word)
}

/// FNV-1a over the record's 64-bit words, the checksum's own word left out.
fn checksum(bytes: &[u8]) -> u64 {
    (0..bytes.len())
        .step_by(8)
        .filter(|&at| at != CHECKSUM)
        .fold(0xcbf2_9ce4_8422_2325, |hash: u64, at| {
            (hash ^ word(bytes, at)).wrapping_mul(0x0000_0100_0000_01b3)
        })
}

fn body_word(seq: u64, index: usize) -> u64 {
    seq.wrapping_add(index as u64)
        .wrapping_mul(0x9e37_79b9_7f4a_7c15)
}

impl Stamp {
    /// The record of `len` bytes, rounded up to a multiple of 8 and to at least [`HEADER`].
    pub fn encode(&self, len: usize) -> Bytes {
        let len = len.max(HEADER).next_multiple_of(8);
        let mut bytes = Vec::with_capacity(len);
        bytes.extend_from_slice(&self.topic.to_le_bytes());
        bytes.extend_from_slice(&self.incarnation.pid.to_le_bytes());
        bytes.extend_from_slice(&self.incarnation.start.to_le_bytes());
        bytes.extend_from_slice(&(len as u32).to_le_bytes());
        bytes.extend_from_slice(&self.seq.to_le_bytes());
        bytes.extend_from_slice(&[0; 8]);
        for index in 0..(len - HEADER) / 8 {
            bytes.extend_from_slice(&body_word(self.seq, index).to_le_bytes());
        }
        let sum = checksum(&bytes);
        bytes[CHECKSUM..CHECKSUM + 8].copy_from_slice(&sum.to_le_bytes());
        Bytes(bytes)
    }

    /// The stamp of a record of `topic`, or what is wrong with the record.
    pub fn decode(bytes: &[u8], topic: u32) -> Result<Self, String> {
        // Not `is_multiple_of`: it is newer than the MSRV, 1.86.
        if bytes.len() < HEADER || bytes.len().next_multiple_of(8) != bytes.len() {
            return Err(format!("a record of {} bytes", bytes.len()));
        }
        let field = |at: usize| {
            u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
        };
        let stamp = Self {
            topic: field(0),
            incarnation: Incarnation {
                pid: field(4),
                start: field(8),
            },
            seq: word(bytes, 16),
        };
        let len = field(12) as usize;
        if len != bytes.len() {
            return Err(format!(
                "seq {}: {} bytes, the record says {len}",
                stamp.seq,
                bytes.len()
            ));
        }
        let sum = word(bytes, CHECKSUM);
        if sum != checksum(bytes) {
            return Err(format!(
                "seq {} of {}: checksum mismatch",
                stamp.seq, stamp.incarnation
            ));
        }
        if stamp.topic != topic {
            return Err(format!(
                "seq {} of topic {:#x}, not {topic:#x}",
                stamp.seq, stamp.topic
            ));
        }
        Ok(stamp)
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

    fn stamp() -> Stamp {
        Stamp {
            topic: topic_id("rsb.fleet.t.frames.1"),
            incarnation: Incarnation {
                pid: 0x0102_0304,
                start: 7,
            },
            seq: u64::MAX - 1,
        }
    }

    #[test]
    fn a_record_survives_its_bytes() {
        for len in [0, HEADER, 33, 4096] {
            let bytes = stamp().encode(len).0;
            assert_eq!(bytes.len(), len.max(HEADER).next_multiple_of(8));
            assert_eq!(Stamp::decode(&bytes, stamp().topic), Ok(stamp()), "{len}");
        }
    }

    #[test]
    fn a_damaged_record_is_refused() {
        let bytes = stamp().encode(64).0;
        for at in 0..bytes.len() {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x10;
            assert!(Stamp::decode(&damaged, stamp().topic).is_err(), "byte {at}");
        }
        assert!(Stamp::decode(&bytes[..56], stamp().topic).is_err());
        assert!(Stamp::decode(&bytes[..63], stamp().topic).is_err());
        assert!(Stamp::decode(&bytes, topic_id("rsb.fleet.t.other.1")).is_err());
    }

    #[test]
    fn topic_ids_differ() {
        assert_ne!(
            topic_id("rsb.fleet.t.frames.1"),
            topic_id("rsb.fleet.t.frames.2")
        );
    }
}

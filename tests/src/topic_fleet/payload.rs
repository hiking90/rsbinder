// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The two payloads of fleet topics, one interface to both: [`Bytes`] with the
//! [`record`](super::record) layout, and the AIDL parcelable [`FleetRecord`] with the same stamp
//! in its fields (T2, T4, T5).

use rsbinder_topic::{Bytes, Payload};

use super::record::{Incarnation, Stamp};

#[allow(
    non_snake_case,
    reason = "the generated AIDL modules are named after their types"
)]
pub mod aidl {
    include!(concat!(env!("OUT_DIR"), "/topic_fleet.rs"));
}

pub use aidl::topicfleet::FleetRecord::FleetRecord;
pub use aidl::topicfleet::IFleetStatus::{BnFleetStatus, IFleetStatus};
pub use aidl::topicfleet::StatusReport::StatusReport;

/// What a fleet publisher writes and a fleet subscriber checks.
pub trait FleetPayload: Payload + Send + 'static {
    /// A value stamped `stamp`; `size` is the length of a `Bytes` record, which a parcelable
    /// ignores.
    fn make(stamp: &Stamp, size: usize) -> Self;

    /// The stamp of a value of `topic`, or what is wrong with it.
    fn stamp(&self, topic: u32) -> Result<Stamp, String>;

    /// The report a T5 record carries: reporter, its incarnation, report seq.
    fn report(&self) -> Option<(String, Incarnation, u64)> {
        None
    }
}

impl FleetPayload for Bytes {
    fn make(stamp: &Stamp, size: usize) -> Self {
        stamp.encode(size)
    }

    fn stamp(&self, topic: u32) -> Result<Stamp, String> {
        Stamp::decode(&self.0, topic)
    }
}

/// FNV-1a over a record's fields, its checksum left out.
fn checksum(record: &FleetRecord) -> i64 {
    let mut words = vec![
        record.topic as u32 as u64,
        record.pid as u32 as u64,
        record.start as u32 as u64,
        record.seq as u64,
        record.values.len() as u64,
    ];
    words.extend(record.values.iter().map(|&value| value as u64));
    if let Some(report) = &record.report {
        words.extend(report.reporter.bytes().map(u64::from));
        words.extend([
            report.reporter.len() as u64,
            report.pid as u32 as u64,
            report.start as u32 as u64,
            report.seq as u64,
        ]);
    }
    let sum = words
        .iter()
        .fold(0xcbf2_9ce4_8422_2325, |hash: u64, &word| {
            (hash ^ word).wrapping_mul(0x0000_0100_0000_01b3)
        });
    sum as i64
}

/// A record stamped `stamp`, carrying `values` and `report`, checksummed.
pub fn fleet_record(stamp: &Stamp, values: Vec<i64>, report: Option<StatusReport>) -> FleetRecord {
    let mut record = FleetRecord {
        topic: stamp.topic as i32,
        pid: stamp.incarnation.pid as i32,
        start: stamp.incarnation.start as i32,
        seq: stamp.seq as i64,
        checksum: 0,
        values,
        report,
    };
    record.checksum = checksum(&record);
    record
}

impl FleetPayload for FleetRecord {
    fn make(stamp: &Stamp, _size: usize) -> Self {
        fleet_record(stamp, Vec::new(), None)
    }

    fn stamp(&self, topic: u32) -> Result<Stamp, String> {
        let stamp = Stamp {
            topic: self.topic as u32,
            incarnation: Incarnation {
                pid: self.pid as u32,
                start: self.start as u32,
            },
            seq: self.seq as u64,
        };
        if self.checksum != checksum(self) {
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

    fn report(&self) -> Option<(String, Incarnation, u64)> {
        let report = self.report.as_ref()?;
        let incarnation = Incarnation {
            pid: report.pid as u32,
            start: report.start as u32,
        };
        Some((report.reporter.clone(), incarnation, report.seq as u64))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp() -> Stamp {
        Stamp {
            topic: 0xfeed,
            incarnation: Incarnation { pid: 9, start: 3 },
            seq: 41,
        }
    }

    fn report() -> StatusReport {
        StatusReport {
            reporter: "prod-1".into(),
            pid: 77,
            start: 1,
            seq: 12,
        }
    }

    #[test]
    fn a_fleet_record_survives_its_check() {
        assert_eq!(FleetRecord::make(&stamp(), 0).stamp(0xfeed), Ok(stamp()));
        let record = fleet_record(&stamp(), vec![3, 4], Some(report()));
        assert_eq!(record.stamp(0xfeed), Ok(stamp()));
        assert!(record.stamp(0xbeef).is_err());
    }

    #[test]
    fn a_changed_field_fails_the_checksum() {
        let changes: [fn(&mut FleetRecord); 7] = [
            |r| r.pid += 1,
            |r| r.seq += 1,
            |r| r.values[1] += 1,
            |r| r.values.push(0),
            |r| r.report = None,
            |r| r.report.as_mut().unwrap().seq += 1,
            |r| r.report.as_mut().unwrap().reporter.push('x'),
        ];
        for (at, change) in changes.iter().enumerate() {
            let mut record = fleet_record(&stamp(), vec![3, 4], Some(report()));
            change(&mut record);
            assert!(record.stamp(0xfeed).is_err(), "change {at}");
        }
    }
}

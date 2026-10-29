// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// The record of the topic fleet's parcelable topics (plan 15 fleet test §5.1):
// the same stamp as a `Bytes` record's header, its fields checksummed.
package topicfleet;

import topicfleet.StatusReport;

parcelable FleetRecord {
    // fnv1a32 of the topic's name.
    int topic;
    // The writer's incarnation.
    int pid;
    int start;
    long seq;
    // FNV-1a over every field but this one.
    long checksum;
    // T4: the seqs of the T1 and T2 records it fused.
    long[] values;
    // T5: the report the relay republished.
    @nullable StatusReport report;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package topicfleet;

import topicfleet.StatusReport;

// The T5 relay of the topic fleet: producers report to it with oneway calls,
// and it republishes each report on its topic (plan 15 fleet test §3.1, F-D5).
interface IFleetStatus {
    oneway void report(in StatusReport report);
}

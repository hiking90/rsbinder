// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package topicfleet;

// What a producer of the topic fleet tells the T5 relay.
parcelable StatusReport {
    // The node's name.
    String reporter;
    // Its incarnation.
    int pid;
    int start;
    // Rising from 0 in each incarnation.
    long seq;
}

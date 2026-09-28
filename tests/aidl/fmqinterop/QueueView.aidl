// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
package fmqinterop;

// One peer's reading of its queue: the two counts its library reports
// and the outcome of a single non-blocking read (`IFmqPeer.view`).
parcelable QueueView {
    // Items the library says can be read, or -1 when it reported an error.
    long availableToRead;
    // Items the library says can be written, or -1 when it reported an error.
    long availableToWrite;
    // 1 when the read returned the items, 0 when the library refused it,
    // -1 when it reported an error.
    int readResult;
    // The sum of the items read when `readResult` is 1.
    long sum;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 12-fmq F2: one end of a Fast Message Queue whose descriptor
// travels over binder. Both halves of the STAGE3 harness implement it —
// rsbinder (`tests/src/fmq_peer.rs`) and AOSP's libfmq on libbinder_ndk
// (`example-hello/cpp/fmq_interop.cpp`) — and either can be the service.
// The caller drives the queue from its own process; the service does
// its half in a handler, so the two blocking calls run on a binder thread.
package fmqinterop;

import android.hardware.common.fmq.MQDescriptor;
import android.hardware.common.fmq.SynchronizedReadWrite;
import fmqinterop.QueueView;

interface IFmqPeer {
    // Make a queue of `capacity` ints with an EventFlag word, keep it,
    // and hand out its descriptor for the caller to attach to.
    MQDescriptor<int, SynchronizedReadWrite> create(int capacity);
    // Attach to a queue the caller made. Returns the capacity this side
    // read from the descriptor.
    int adopt(in MQDescriptor<int, SynchronizedReadWrite> desc);
    // Write 0..count into the queue, waiting for room; returns the count
    // written. A queue that stays full for ten seconds ends the call
    // with a service-specific error.
    int produce(int count);
    // Read `count` items, waiting for them, and return their sum. Fails
    // the same way as `produce`.
    long consume(int count);
    // What the queue reports right now, plus one non-blocking read of
    // `count` items. Nothing waits; the library's own answer is returned
    // as it is, so a caller that damaged the counters can record it.
    QueueView view(int count);
}

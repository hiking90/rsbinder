// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-7 fixture: a service that streams integers into an endpoint
// the caller supplies.
//
// `rsbinder.stream.StreamEndpoint` resolves to `rsbinder::stream::
// StreamEndpoint` — rsbinder-aidl ships the declaration — so the service
// opens a `Sink` on exactly what the caller's `Receiver` made. Which
// transport the stream runs on is decided by the caller's peer: over
// kernel binder the endpoint carries a ring, over an RPC session only the
// sink. The methods return nothing: the stream's own records or calls
// carry everything.
package streamdemo;

import rsbinder.stream.StreamEndpoint;

interface IStreamDemo {
    // Stream `count` integers, starting at 0, into `endpoint`.
    // `maxBatchBytes` and `initialCredits` shape the RPC path (batches
    // hold at most `maxBatchBytes`, the producer opens with
    // `initialCredits` of them) and are ignored on a ring. `delayMicros`
    // paces the producer, which is how a test decides whether a consumer
    // that dies finds it holding room or parked waiting for some.
    void subscribe(in StreamEndpoint endpoint, int count, int maxBatchBytes,
                   int initialCredits, int delayMicros);

    // `subscribe`, with the producer running as a task on the service's
    // async runtime instead of a thread of its own.
    void subscribeAsync(in StreamEndpoint endpoint, int count, int maxBatchBytes,
                        int initialCredits);

    // Stream `count` integers and then end with a service-specific
    // failure carrying `code` and `message`.
    void subscribeFailing(in StreamEndpoint endpoint, int count, int code, String message);

    // How many items the producer has handed to the stream so far.
    int sent();

    // Whether the producer thread has left its loop. A producer that
    // neither finished the stream nor was cancelled is still parked on
    // room or on credit, so this is what tells a released producer from
    // a stuck one.
    boolean finished();

    // The `StatusCode` the last `send` or `end` failed with, as its
    // integer value, or 0 while nothing has failed. A consumer that dies
    // mid-stream is only visible to the producer this way, and only on
    // kernel binder — which is why `run_stream_ac.sh` exists.
    int lastError();
}

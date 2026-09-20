// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-7 fixture: a service that streams integers into a sink the
// caller supplies.
//
// The sink and the source are `IBinder` rather than
// `rsbinder.stream.IStreamSink` / `IStreamSource`: every crate that
// compiles those AIDLs gets its own Rust types, so importing them here
// would produce a second pair incompatible with the ones inside
// `rsbinder::stream` (the wire is identical either way). That is also
// the shape `rsbinder::stream` asks for — `Sink::new` and
// `Receiver::attach_source` both take a bare binder.
package streamdemo;

interface IStreamDemo {
    // Stream `count` integers, starting at 0, into `sink`. Batches hold
    // at most `maxBatchBytes`, and the producer opens with
    // `initialCredits` of them. `delayMicros` paces the producer, which
    // is how a test decides whether a consumer that dies finds it holding
    // credit or parked waiting for some. Returns the source to grant
    // credit on.
    IBinder subscribe(IBinder sink, int count, int maxBatchBytes, int initialCredits,
                      int delayMicros);

    // Stream `count` integers and then end with a service-specific
    // failure carrying `code` and `message`.
    IBinder subscribeFailing(IBinder sink, int count, int code, String message);

    // How many items the producer has handed to the sink so far.
    int sent();

    // Whether the producer thread has left its loop. A producer that
    // neither finished the stream nor was cancelled is still parked on
    // credit, so this is what tells a released producer from a stuck one.
    boolean finished();

    // The `StatusCode` the last `send` or `end` failed with, as its
    // integer value, or 0 while nothing has failed. A consumer that dies
    // mid-stream is only visible to the producer this way, and only on
    // kernel binder — which is why `run_stream_ac.sh` exists.
    int lastError();
}

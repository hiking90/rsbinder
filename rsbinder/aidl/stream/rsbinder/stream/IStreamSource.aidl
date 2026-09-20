/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.stream;

/**
 * The producer end of a stream, as the consumer sees it: this is where
 * the consumer grants the producer permission to send more, and where it
 * says it wants no more at all.
 *
 * The service hands this object to the consumer alongside taking its
 * IStreamSink — typically as the return value of the method that starts
 * the stream.
 */
interface IStreamSource {
    /**
     * Grant `credits` more batches.
     *
     * Credits are counted in IStreamSink.onBatch calls, not in items or
     * bytes, and they add up: two grants of 4 leave the producer able to
     * send 8 batches. The producer caps what one batch may hold, so a
     * granted window bounds the bytes in flight as well as the calls.
     *
     * `twoway`, so a return also tells the consumer the producer is
     * still there. It travels consumer to producer, the opposite
     * direction from IStreamSink, so it neither needs nor disturbs the
     * ordering the sink relies on.
     */
    void request(int credits);

    /**
     * No more items are wanted.
     *
     * The producer stops at its next batch boundary and is released from
     * any wait for credit. It may still send IStreamSink.onEnd; a
     * consumer that has cancelled can ignore it.
     */
    oneway void cancel();
}

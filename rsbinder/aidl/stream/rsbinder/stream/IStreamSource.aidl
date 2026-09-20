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
 * The producer hands this object to the consumer in
 * IStreamSink.onStart. Both methods are `oneway`: a grant is a
 * notification, not a question, and a consumer that had to wait for the
 * producer's reply before taking its next batch would be paced by how
 * busy the producer's threads are.
 */
interface IStreamSource {
    /**
     * Grant `credits` more batches.
     *
     * Credits are counted in IStreamSink.onBatch calls, not in items or
     * bytes, and they add up: two grants of 4 leave the producer able to
     * send 8 batches. A producer sends a batch once it reaches a byte
     * threshold, so a granted window bounds the bytes in flight only
     * for items smaller than that threshold: a larger item still goes
     * out, in a batch of its own.
     *
     * `credits` must be positive; a producer ignores a grant that is
     * not. A grant that cannot be delivered fails at the sender — the
     * driver reports a dead or full receiver on a `oneway` call too — so
     * a consumer keeps the credit and grants it again later.
     *
     * A consumer grants again for every batch it drains, for as long as
     * it wants more: a producer that runs out of credit sends nothing
     * further, and nothing else will prompt it to. There is no timeout on
     * that wait, and with both processes alive no death link ends it
     * either.
     *
     * It travels consumer to producer, the opposite direction from
     * IStreamSink, so it neither needs nor disturbs the ordering the
     * sink relies on.
     */
    oneway void request(int credits);

    /**
     * No more items are wanted.
     *
     * The producer stops at its next batch boundary and is released from
     * any wait for credit. It may still send IStreamSink.onEnd; a
     * consumer that has cancelled can ignore it.
     */
    oneway void cancel();
}

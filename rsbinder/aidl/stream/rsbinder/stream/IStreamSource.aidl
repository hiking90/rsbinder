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
 *
 * This interface is the RPC path only: on a StreamEndpoint that carries
 * a ring the consumer cancels through the ring's EventFlag, and there is
 * no credit to grant, so this object is never made. IStreamSink.aidl has
 * what the session needs to carry a stream.
 */
interface IStreamSource {
    /**
     * The consumer has now granted `total` batches in all, since the
     * stream began and beyond the window the producer opened with.
     *
     * A running total, not an amount to add. The producer keeps the
     * highest total it has seen and gains the difference: after
     * `request(4)`, a `request(6)` is worth two more batches, and a
     * second `request(4)` — or any total not above the highest — is
     * worth none. That makes a grant safe to send again. A `oneway` call
     * that reports failure to its sender has almost always not arrived,
     * but a sender cannot always tell, and an amount to add would then
     * be counted twice or not at all; a total is counted once whichever
     * happened. It is also why the two ends can agree on how many
     * batches the producer may send, which IStreamSink.onStart relies
     * on.
     *
     * Credits are counted in IStreamSink.onBatch calls, not in items or
     * bytes. A producer sends a batch once it reaches a byte threshold,
     * so a granted window bounds the bytes in flight only for items
     * smaller than that threshold: a larger item still goes out, with
     * the batch it was added to.
     *
     * A grant that cannot be delivered fails at the sender — the session
     * has ended, or it has no free connection to send on — and the
     * consumer sends its total again later, by then perhaps a larger
     * one. A total that is not positive is ignored.
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
    oneway void request(long total);

    /**
     * No more items are wanted.
     *
     * The producer sends no further IStreamSink.onBatch — items it has
     * not yet sent are dropped — and is released from any wait for
     * credit. It may still send IStreamSink.onEnd; a consumer that has
     * cancelled can ignore it.
     *
     * A cancel is also how a consumer that refused the stream says so
     * (IStreamSink.onStart, IStreamSink.onBatch): the reason stays on the
     * consumer's side. And a consumer that closes cancels even after
     * IStreamSink.onEnd, so a producer takes a cancel that arrives after
     * its end as nothing.
     */
    oneway void cancel();
}

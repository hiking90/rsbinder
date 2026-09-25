/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.stream;

import android.hardware.common.fmq.MQDescriptor;
import android.hardware.common.fmq.SynchronizedReadWrite;

/**
 * The consumer's end of a stream, made by the consumer and handed to the
 * producer in the one call that opens the stream: as an argument when
 * the consumer is the caller (a download), as the return value when the
 * producer is (an upload).
 *
 * Which of the two fields carries the items depends on the transport the
 * two ends share, and the consumer decides when it makes the endpoint.
 */
parcelable StreamEndpoint {
    /**
     * Kernel binder only: the ring the producer writes records into and
     * the consumer reads them out of, a synchronized Fast Message Queue
     * of bytes with an EventFlag word. Absent when the two ends are on an
     * RPC session, which carries no shared memory.
     *
     * A record is a 4-byte little-endian header followed by its payload.
     * The header's top bit is the kind — 0 for an item, 1 for the end of
     * the stream — and its low 31 bits are the payload length. An item's
     * payload is one value in parcel encoding, with no binder and no file
     * descriptor; the end's is `int exception`, `int serviceSpecific`
     * (little-endian) and the rest a UTF-8 message, absent when the
     * payload is exactly eight bytes. The three are the arguments of
     * IStreamSink.onEnd. Nothing follows the end record.
     *
     * The producer writes each record with one commit, so the consumer
     * only ever sees whole records, and keeps the last 256 bytes of the
     * ring free of items — an item record is written only while the
     * bytes in flight plus its own size stay within `capacity - 256` — so
     * that the end record (at most 256 bytes, its message cut to fit)
     * always has room and is never waited on. An item larger than
     * `capacity - 256 - 4` bytes cannot be sent at all.
     *
     * EventFlag bits: NOT_FULL (1) is set by the consumer after each
     * read and waited on by the producer; NOT_EMPTY (2) is set by the
     * producer after each write and waited on by the consumer; CANCEL (4)
     * is set by the consumer when it wants no more and is waited on —
     * and so consumed — by the producer only. A producer waits with the
     * mask `NOT_FULL | CANCEL`, never with `writeBlocking`, which treats
     * every wake as a retry and would not see the cancel.
     */
    @nullable MQDescriptor<byte, SynchronizedReadWrite> ring;

    /**
     * The consumer's IStreamSink. Over an RPC session the producer calls
     * it — `onStart`, then batches, then `onEnd` — and it is the whole of
     * the stream; over kernel binder the producer only links to its death,
     * and makes no call on it.
     */
    IBinder sink;
}

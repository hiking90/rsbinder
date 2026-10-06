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
 * two ends share, and the consumer decides when it makes the endpoint:
 * it makes a ring when the binder it holds in the producer's process is a
 * kernel binder object, and leaves `ring` absent when that binder is an
 * RPC proxy — unless the consumer opts in and the RPC session is a Unix
 * socket on the same host that passes file descriptors and carries calls
 * both ways, when it makes a ring there too. The producer follows the
 * endpoint: a present `ring` is the ring path, an absent one the calls
 * path (IStreamSink.aidl). A producer on an RPC session that does not
 * implement the ring refuses an endpoint that carries one.
 *
 * Each end links to the other's binder for death: the producer to `sink`,
 * the consumer to a binder in the producer's process — the service it
 * called in a download, the binder the producer passed in an upload.
 * Back-pressure leaves most of a stream with no call in flight that could
 * fail, so nothing else tells a waiting end that its peer is gone.
 *
 * `T` is the stream's item type, named where the endpoint is used —
 * `void subscribe(in StreamEndpoint<LogLine> endpoint)` — so that both
 * ends and a reader of the interface know what the items are. It is not
 * written to the parcel: the endpoint is the same bytes whatever `T` is,
 * and the items travel on the ring or the sink below, each one encoded
 * as the generated code writes an `in` argument of type `T`. rsbinder-aidl
 * refuses an array or a `List` as `T` (AOSP's aidl accepts either); wrap
 * one in a parcelable.
 */
parcelable StreamEndpoint<T> {
    /**
     * The ring the producer writes records into and the consumer reads
     * them out of — a synchronized Fast Message Queue of `byte` (`int8_t`,
     * quantum 1) with an EventFlag word, allocated by the consumer.
     * Present on kernel binder; on an RPC session only when the consumer
     * opted in and the session is a Unix socket on the same host, where
     * the memfd crosses as an `SCM_RIGHTS` fd and a peer's death is the
     * session ending. Absent otherwise: an RPC session in general carries
     * no shared memory.
     *
     * A record is a 4-byte little-endian header followed by its payload.
     * The header's top bit is the kind — 0 for an item, 1 for the end of
     * the stream — and its low 31 bits are the payload length.
     *
     * An item's payload is one value in parcel encoding: exactly what the
     * generated code writes for an `in` argument of the item type, so a
     * parcelable starts with its non-null marker (`int` 1). It has no
     * object table, so it holds no binder and no file descriptor.
     *
     * The end's payload is `int exception`, `int serviceSpecific`
     * (little-endian) and the rest a UTF-8 message, absent when the
     * payload is exactly eight bytes. The three are the fields of an AOSP
     * `binder::Status`, as in IStreamSink.onEnd: `exception` is `0`
     * (`EX_NONE`, the stream ran out) or an `EX_*` code from `-1` to `-9`,
     * and `serviceSpecific` means something only when `exception` is `-8`
     * (`EX_SERVICE_SPECIFIC`). A consumer ends the stream with an error of
     * its own for any other `exception`, and reads the message with
     * invalid UTF-8 replaced. Nothing follows the end record.
     *
     * The producer writes each record with one commit, so the consumer
     * only ever sees whole records, and keeps the last 256 bytes of the
     * ring free of items — an item record is written only while the
     * bytes in flight plus its own size stay within `capacity - 256` — so
     * that the end record (at most 256 bytes, its message cut to fit)
     * always has room and is never waited on. An item larger than
     * `capacity - 256 - 4` bytes cannot be sent at all, and a ring of
     * `256 + 4` bytes or fewer carries none.
     *
     * EventFlag bits: NOT_FULL (1) is set by the consumer after each
     * read and waited on by the producer; NOT_EMPTY (2) is set by the
     * producer after each write and waited on by the consumer; CANCEL (4)
     * is set by the consumer when it wants no more and is waited on —
     * and so consumed — by the producer only. A producer waits with the
     * mask `NOT_FULL | CANCEL`, never with `writeBlocking`, which treats
     * every wake as a retry and would not see the cancel.
     *
     * Each end wakes after every read or write it commits, not only when
     * the ring stops being full or empty: a libfmq writer waits whenever
     * `availableToWrite()` is below what it needs, full or not. A consumer
     * that reads several records in one call may wake once for all.
     *
     * CANCEL: the producer checks the bit before writing each item record
     * — libfmq has no peek, so it loads `getEventFlagWord()` — and keeps a
     * CANCEL it saw in its own state, because a wait clears the bits it
     * returns. From then on it writes no item. CANCEL does not stop the
     * end record: the producer may still write one, and the consumer reads
     * it. The consumer never puts CANCEL in its own wait mask, which would
     * clear the bit it set for the producer.
     *
     * Death: the death callback sets a dead flag of its own end and wakes
     * with its own end's wait mask — NOT_FULL on the producer, NOT_EMPTY
     * on the consumer — so the woken waiter finds the flag; no bit in the
     * shared word means death. The consumer reads its dead flag before it
     * looks at the ring, and ends the stream as dead only when that look
     * finds the ring empty: records written before the death are still
     * delivered, and an end record already in the ring is the stream's
     * end.
     *
     * Closing: a consumer that closes sets CANCEL, also after a clean
     * end. A producer that closes without having written an end record
     * writes one with `EX_ILLEGAL_STATE` (`-5`), so the consumer does not
     * wait for an end that never comes.
     *
     * The ring is the other process's memory, and each end checks what it
     * takes from it. Before mapping, the producer requires every fd to be
     * a memfd sealed with `F_SEAL_SHRINK` or an ashmem region, so the
     * consumer cannot shrink it under the mapping; the EventFlag word; and
     * a capacity above `256 + 4` and no larger than it is willing to map
     * (rsbinder's default is 4 MiB). It refuses the endpoint otherwise.
     * The consumer ends the stream with `EX_ILLEGAL_STATE` and sets CANCEL
     * when a header claims an item payload above `capacity - 256 - 4`
     * bytes, an end payload below 8 or above `capacity - 4` bytes, or more
     * bytes than are in the ring; when a payload does not decode as one
     * item it ends the stream with that error and sets CANCEL too. Either
     * end treats counters outside `read <= write <= read + capacity` as a
     * broken ring: the consumer ends the stream as above, the producer
     * refuses every later write.
     *
     * A libfmq peer attaches to a ring it did not make with
     * `AidlMessageQueue(desc, resetPointers = false)`: the default resets
     * both counters, which discards any record already written. A ring
     * serves one stream — the other end may keep it mapped after the
     * stream ends — so a consumer never hands it to a second producer.
     *
     * Across SELinux domains the producer maps a memfd the consumer's
     * domain made, and the base policy grants `memfd_file` access only on
     * a domain's own. The producer's domain then needs
     * `memfd_file { getattr read write map }` on the consumer's domain,
     * besides the `fd use` that any fd passed between the two needs, or
     * the stream cannot open.
     */
    @nullable MQDescriptor<byte, SynchronizedReadWrite> ring;

    /**
     * The consumer's IStreamSink. On both paths the producer links to its
     * death. With no `ring` the producer also calls it — `onStart`, then
     * batches, then `onEnd` — and it is the whole of the stream. With a
     * `ring` no method on it is called; a producer that calls `onStart` or
     * `onBatch` on it ends the stream with `EX_ILLEGAL_STATE`, after the
     * records already in the ring.
     */
    IBinder sink;
}

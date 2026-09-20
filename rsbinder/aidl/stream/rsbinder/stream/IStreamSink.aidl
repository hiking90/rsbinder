/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.stream;

/**
 * The consumer end of a stream: the producer calls this to deliver items
 * and to say the stream is over.
 *
 * rsbinder distributes this file so a C++ or Java peer can take part
 * without rsbinder on its side. Every method is `oneway`, which is what
 * keeps the calls in order: the kernel orders `oneway` calls to one node
 * against each other, but not against a `twoway` to a different node, so
 * a reply-carrying method here would let `onEnd` overtake the batches it
 * is supposed to follow.
 *
 * The shape is the reactive-streams one — `onStart` / `onBatch` /
 * `onEnd` here are `onSubscribe` / `onNext` / `onComplete`-or-`onError`
 * there, and IStreamSource is the `Subscription`.
 *
 * Flow control is not here — it runs the other way, over IStreamSource.
 */
interface IStreamSink {
    /**
     * The stream exists, and `source` is where to grant credit and to
     * cancel.
     *
     * Sent once, before any batch. `source` is an IStreamSource; it is
     * declared `IBinder` so that a peer casts it itself
     * (`IStreamSource::asInterface`, `IStreamSource.Stub.asInterface`)
     * rather than having the generated stub do so — the wire is the same
     * binder object either way.
     *
     * The consumer should watch `source` for death from here on: with
     * back-pressure in play there is usually no call in flight to fail,
     * so a producer that dies is otherwise indistinguishable from one
     * with nothing to send yet.
     *
     * A producer may already hold an opening window of credit and send
     * batches right behind this call, without waiting for a grant.
     */
    oneway void onStart(IBinder source);

    /**
     * One batch of items.
     *
     * `items` is the items' IPC bytes, one after another, exactly as a
     * parcel holds them; `count` says how many are in there. A peer
     * decodes them by putting the array into a parcel — C++
     * `Parcel::setData`, Java `Parcel.unmarshall` followed by
     * `setDataPosition(0)`, NDK `AParcel_unmarshal` followed by
     * `AParcel_setDataPosition(p, 0)` — and then reading the item type
     * `count` times.
     *
     * The bytes carry no binder and no file descriptor. Neither means
     * anything in the receiving process, so the producer refuses an item
     * containing one rather than sending a number that would be a lie.
     *
     * Each call spends one credit granted through
     * IStreamSource.request.
     */
    oneway void onBatch(in byte[] items, int count);

    /**
     * The stream is over, and no further call arrives on this object.
     *
     * The three arguments are the fields of an AOSP `binder::Status`
     * (`frameworks/native/libs/binder/Status.h`), spelled out because
     * AIDL has no `Status` parameter type:
     *
     *  - `exception` is an `EX_*` code, and `0` (`EX_NONE`) means the
     *    stream ended because it ran out, not because anything failed.
     *  - `serviceSpecific` carries the service's own error code, and is
     *    meaningful only when `exception` is `-8`
     *    (`EX_SERVICE_SPECIFIC`). Ignore it otherwise.
     *  - `message` is the status message, absent when the producer set
     *    none.
     *
     * A C++ peer rebuilds the status with
     * `Status::fromServiceSpecificError(serviceSpecific, message)` when
     * `exception` is `-8`, and `Status::fromExceptionCode(exception,
     * message)` otherwise.
     *
     * `EX_TRANSACTION_FAILED` (`-129`) is never sent: it reports that
     * the binder layer itself failed, which is not something a call that
     * arrived can say. `-127` and `-128` are reply-header markers and
     * are likewise never sent here.
     *
     * Spending no credit, so a stream can end even with the window
     * closed.
     */
    oneway void onEnd(int exception, int serviceSpecific, @nullable String message);
}

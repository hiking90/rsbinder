/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.stream;

/**
 * The consumer end of a stream over an RPC session: the producer calls
 * this to deliver items and to say the stream is over.
 *
 * This interface is the calls path only. A StreamEndpoint that carries a
 * ring — always against a kernel binder peer, and on a Unix RPC session
 * when the consumer opts in — has the items travel on it, and this
 * object, still the endpoint's `sink`, is only linked to for death; a
 * producer that calls `onStart` or `onBatch` on it ends the stream.
 * StreamEndpoint.aidl has the ring's layout.
 *
 * rsbinder distributes this file so a C++ or NDK peer can take part
 * without rsbinder on its side. Every method is `oneway`, which is what
 * keeps the calls in order: a session orders `oneway` calls to one
 * object against each other, so a reply-carrying method here would let
 * `onEnd` overtake the batches it is supposed to follow.
 *
 * Whichever end is the server calls the client outside any handler — the
 * producer's `onStart`, batches and `onEnd` in a download, the
 * consumer's grants and cancel in an upload — so the client must open
 * incoming connections on the session (libbinder
 * `ARpcSession_setMaxIncomingThreads`); a session without them carries
 * no stream. A `oneway` call made from inside a handler is no exception:
 * it never goes back over the connection that delivered the call, so it
 * needs an incoming connection too. Death over RPC is the session
 * ending, and it too reaches a client only through those connections.
 * The producer links to this object's death, and the consumer to
 * `source`'s (see `onStart`).
 *
 * The shape is the reactive-streams one — `onStart` / `onBatch` /
 * `onEnd` here are `onSubscribe` / `onNext` / `onComplete`-or-`onError`
 * there, and IStreamSource is the `Subscription`.
 *
 * Flow control is not here — it runs the other way, over IStreamSource.
 */
interface IStreamSink {
    /**
     * The stream exists, `source` is where to grant credit and to
     * cancel, and `credits` is the window the producer opens with.
     *
     * Sent once, before any batch. `source` is an IStreamSource; it is
     * declared `IBinder` so that a peer casts it itself (C++
     * `IStreamSource::asInterface`, NDK `IStreamSource::fromBinder`)
     * rather than having the generated stub do so — the wire is the same
     * binder object either way.
     *
     * The consumer should watch `source` for death from here on: with
     * back-pressure in play there is usually no call in flight to fail,
     * so a producer that dies is otherwise indistinguishable from one
     * with nothing to send yet.
     *
     * A producer opens with `credits` batches of credit of its own and
     * sends them right behind this call, without waiting for a grant. It
     * has to open with at least one: a consumer grants credit for
     * batches it has drained, so a producer that starts at zero would
     * wait for a grant that nothing can trigger.
     *
     * The window is stated so that the consumer can hold the producer to
     * it. Everything the producer may send is this number plus what
     * IStreamSource.request has granted since, and a consumer ends the
     * stream on a batch beyond that rather than queue without bound. A
     * consumer also has a largest window it accepts, and ends the stream
     * here, before any batch, when `credits` is above it or is not
     * positive — so a producer that wants a wide window needs a consumer
     * that was set up to take one. Such a stream ends with
     * `EX_ILLEGAL_ARGUMENT` on the consumer's side, and the consumer
     * sends `source` IStreamSource.cancel: being `oneway`, this call has
     * no other way to tell the producer.
     *
     * A second `onStart` is ignored; one from a different `source` is
     * answered with a cancel to that source.
     */
    oneway void onStart(IBinder source, int credits);

    /**
     * One batch of items.
     *
     * `items` is the items' IPC bytes, one after another, exactly as a
     * parcel holds them; `count` says how many are in there. A peer
     * decodes them by putting the array into a parcel — C++
     * `Parcel::setData`, NDK `AParcel_unmarshal` followed by
     * `AParcel_setDataPosition(p, 0)` — and then reading the item type
     * `count` times. Each item is encoded as the generated code writes an
     * `in` argument of its type, so a parcelable starts with its non-null
     * marker (`int` 1).
     *
     * The bytes carry no binder and no file descriptor. Neither means
     * anything in the receiving process, so the producer refuses an item
     * containing one rather than sending a number that would be a lie.
     *
     * Each call spends one credit, of the opening window or of those
     * granted through IStreamSource.request. A call made with none left
     * — a batch sent before `onStart` included — ends the stream with
     * `EX_ILLEGAL_STATE`. A negative `count` ends it with
     * `EX_ILLEGAL_ARGUMENT`, and bytes that do not decode as exactly
     * `count` items end it with the decode error; each of these also
     * sends IStreamSource.cancel. A batch that arrives after the stream
     * has ended, or after the consumer closed, is dropped.
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
     * are likewise never sent here. The valid codes are `0` and `-1` to
     * `-9`; a consumer ends the stream with an error of its own on any
     * other.
     *
     * Spending no credit, so a stream can end even with the window
     * closed.
     */
    oneway void onEnd(int exception, int serviceSpecific, @nullable String message);
}

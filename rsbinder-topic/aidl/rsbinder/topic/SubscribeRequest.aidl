/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/**
 * What a subscriber hands the publisher in ITopic.subscribe: who it is, what
 * it expects the topic to carry, and the one piece of shared memory it
 * writes.
 */
parcelable SubscribeRequest {
    /**
     * The layout version the subscriber reads and writes,
     * `major << 16 | minor`. While the major is 0 the publisher takes only
     * its own value and refuses any other with
     * ITopic.ERROR_VERSION.
     */
    int version;

    /**
     * The name of the payload type the subscriber decodes. The publisher
     * compares it with the name of its own type and refuses a different one
     * with `BAD_TYPE`. In rsbinder it is the parcelable's descriptor, or
     * the name a custom `Payload` gives.
     */
    String type;

    /**
     * A binder of the subscriber's own process. The publisher links to its
     * death, which is how it learns that the subscriber's process is gone.
     * A binder of the publisher's own process is refused with `BAD_VALUE`,
     * as its death says nothing about the caller, unless the call came from
     * that process itself. A binder of a third process cannot be told from
     * the caller's own; `ITopic.ERROR_FULL` per uid bounds what a caller
     * holds that way. It is an `ISubscription`, refused with `BAD_VALUE`
     * otherwise: a topic with backpressure tells the subscriber through it
     * that the subscription was taken out.
     */
    IBinder token;

    /**
     * The subscriber's control page: a memfd sealed with `F_SEAL_GROW`,
     * `F_SEAL_SHRINK`, `F_SEAL_FUTURE_WRITE` and `F_SEAL_SEAL`, every page
     * written. The publisher maps it read-only and refuses, with
     * `BAD_VALUE`, an fd that is not such a memfd or whose size, magic,
     * version or pin count is not what this request says.
     */
    ParcelFileDescriptor control;

    /**
     * Pin words at the end of the control page, from offset 0x80: 0 or 1,
     * and the publisher refuses any other value with `BAD_VALUE`. A pin word
     * is 0, or the number of the pool slot the subscriber reads plus one;
     * the publisher does not lend a slot a pin word names. A subscriber
     * without a pin word cannot read the values of a publisher with a pool.
     */
    int pins;

    /**
     * How the publisher wakes the subscriber: ITopic.WAKE_FUTEX, a futex on
     * the topic memory's `seq` that one wake per commit serves for every
     * such subscriber, or ITopic.WAKE_FD, a byte on a socket of this
     * subscription alone, which the subscriber can poll with other fds. The
     * publisher refuses any other value with `BAD_VALUE`.
     */
    int wake;
    /**
     * For ITopic.WAKE_FD, one end of a unix stream socket pair the
     * subscriber made; null otherwise, and `BAD_VALUE` if either is not so.
     * The publisher sends one byte on it, without waiting, when a commit
     * finds the subscriber waiting, and takes the subscription out once its
     * sends fail with `EPIPE`: closing the other end ends the subscription.
     * The subscriber keeps this end too, to wake its own waits through the
     * same socket.
     */
    @nullable ParcelFileDescriptor wakeFd;
}

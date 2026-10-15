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
     * holds that way.
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
     * Pin words at the end of the control page. 0: this version has no
     * loans, and the publisher refuses any other value with `BAD_VALUE`.
     */
    int pins;
}

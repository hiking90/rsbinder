/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.SubscribeRequest;
import rsbinder.topic.TopicEndpoint;

/**
 * A topic: one publisher, any number of subscribers. A subscription is this
 * one call; the records then travel through shared memory with no further
 * binder transaction.
 *
 * The payload type is not part of the signature, so one interface serves
 * every topic; the request names the type and the publisher compares it.
 */
interface ITopic {
    /** SubscribeRequest.wake: a futex on the topic memory's `seq`. */
    const int WAKE_FUTEX = 0;
    /** SubscribeRequest.wake: a byte on TopicEndpoint.wakeFd. */
    const int WAKE_FD = 1;

    /** ServiceSpecific: the request's layout version is not the publisher's. */
    const int ERROR_VERSION = 1;
    /**
     * ServiceSpecific: the publisher has as many subscribers as it takes, in
     * all or from the caller's uid.
     */
    const int ERROR_FULL = 2;
    /** ServiceSpecific: the publisher has been closed. */
    const int ERROR_CLOSED = 3;

    /**
     * Take a subscription. A publisher that refuses the caller returns
     * `EX_SECURITY`; the other refusals are the constants above, `BAD_TYPE`
     * for a payload type of another name and `BAD_VALUE` for a request it
     * cannot use (SubscribeRequest names each).
     */
    TopicEndpoint subscribe(in SubscribeRequest request);
}

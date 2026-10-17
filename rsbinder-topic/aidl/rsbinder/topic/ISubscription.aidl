/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/**
 * The subscriber's end of one subscription: the binder it sends as
 * `SubscribeRequest.token`, which the publisher links to the death of and
 * calls back on.
 */
interface ISubscription {
    /**
     * The publisher took this subscription out: a topic with backpressure
     * waited for it past its timeout. The publisher no longer keeps the
     * records this subscriber has not read; the next receive ends the
     * subscription. Sent once, by the publisher only.
     */
    oneway void evicted();
}

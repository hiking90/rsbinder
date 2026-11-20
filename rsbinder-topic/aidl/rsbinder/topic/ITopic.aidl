/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.SubscribeRequest;
import rsbinder.topic.TopicEndpoint;
import rsbinder.topic.TopicStatus;

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
    /** SubscribeRequest.wake: a byte on SubscribeRequest.wakeFd. */
    const int WAKE_FD = 1;

    /**
     * TopicEndpoint.topicKind: each record tells of something that happened
     * once, and a subscriber reads them in order.
     */
    const int TOPIC_EVENT = 0;
    /**
     * TopicEndpoint.topicKind: the newest record is the current value, and a
     * subscriber reads the newest.
     */
    const int TOPIC_STATE = 1;

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
     * SubscribeRequest.flags, TopicEndpoint.flags: the publisher does not
     * wait for this subscriber. A publish does not wait for its reads, a
     * pool lends a slot it has not read past unless it pins it, and a wait
     * for room never takes it out; it loses what it has not read in time.
     */
    const int FLAG_UNWAITED = 1;

    /**
     * Take a subscription. A publisher that refuses the caller returns
     * `EX_SECURITY`; the other refusals are the constants above, `BAD_TYPE`
     * for a payload type of another name, `UNEXPECTED_NULL` for a null
     * `token` or `control`, and `BAD_VALUE` for a request it cannot use
     * (SubscribeRequest names each).
     */
    TopicEndpoint subscribe(in SubscribeRequest request);

    /**
     * A subscriber of this process has dropped a subscription: the publisher
     * takes out the subscriptions whose control page is marked closed now,
     * not at its next publish. It carries nothing and changes nothing else,
     * so any caller may make it. A publisher that predates it answers
     * `UNKNOWN_TRANSACTION`, which a one-way caller never sees.
     */
    oneway void unsubscribed();

    /**
     * The topic and its subscriptions now. It names other processes'
     * uids and pids, so a publisher answers only the callers its policy
     * lets look, and refuses the rest with `EX_SECURITY`. A publisher that
     * predates it answers `UNKNOWN_TRANSACTION`.
     */
    TopicStatus status();
}

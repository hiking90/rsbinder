/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.SubscriberStatus;

/** A topic as its publisher sees it: ITopic.status's reply. */
parcelable TopicStatus {
    /** The name of the payload type (TopicEndpoint.typeName). */
    String typeName;
    /** ITopic.TOPIC_EVENT or ITopic.TOPIC_STATE. */
    int topicKind;
    /** Bytes of ring. */
    long ringBytes;
    /** Whether a publish waits for its subscribers' reads. */
    boolean lossless;
    /** Records published, the end record left out, and their payload bytes. */
    long published;
    long bytes;
    /** Subscriptions a wait for room took out. */
    long evicted;
    /** The subscriptions held, after taking out those that ended. */
    SubscriberStatus[] subscribers;
}

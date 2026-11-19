/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/**
 * One subscription as its publisher holds it, in ITopic.status's reply.
 * `lagBytes`, `lost`, `waiting` and `pinned` are what the subscriber wrote
 * in its control page: what it reports.
 */
parcelable SubscriberStatus {
    /** The publisher's number for it, the bits of a `u64` (TopicEndpoint.id). */
    long id;
    /** The uid and pid of the process that made the subscription call. */
    int uid;
    int pid;
    /** Bytes of ring between its read position and the write position. */
    long lagBytes;
    /** Records it was told it lost. */
    long lost;
    /** Whether it is about to sleep, or sleeps, for the next record. */
    boolean waiting;
    /** Whether a wait for room took it out. */
    boolean evicted;
    /** Whether it asked not to be waited for (ITopic.FLAG_UNWAITED). */
    boolean unwaited;
    /** The pool slot it pins, or -1 for none. */
    int pinned;
}

/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/** One name with a topic, as `ITopicRegistry.list` returns it. */
parcelable TopicInfo {
    String name;
    /** The uid and pid that advertised the topic. */
    int uid;
    int pid;
    /** Milliseconds since the name was given to its topic, on the registry's monotonic clock. */
    long ageMillis;
    /** Watchers of the name. */
    int watchers;
}

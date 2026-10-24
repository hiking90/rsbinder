/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.ITopic;

/** A name's topic and the generation of the change that gave it. */
parcelable TopicRef {
    /** Null when the name has no topic. */
    @nullable ITopic topic;
    /**
     * The registry's generation when the name was given to `topic`; with no
     * topic, the generation when the answer was made.
     */
    long generation;
}

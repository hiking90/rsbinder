/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/** Where a name's topic can be reached, and the generation of the change that gave it. */
parcelable TopicAddress {
    /** An `O_PATH` descriptor of the socket's file; null when the name has no topic. */
    @nullable ParcelFileDescriptor socket;
    /**
     * The registry's generation when the name was given to this socket; with no
     * socket, the generation when the answer was made, or `Long.MIN_VALUE` when
     * the caller may not subscribe to the name.
     */
    long generation;
}

/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/** A socket `ITopicUdsRegistry.allocate` made for a publisher. */
parcelable TopicAllocation {
    /** The bound Unix stream socket, not listening; the registry keeps no copy of it. */
    ParcelFileDescriptor socket;
    /** Which allocation of the name this is, for `commit` and `withdraw`. */
    long allocation;
}

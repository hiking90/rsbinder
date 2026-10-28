/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/**
 * The pool of a topic whose publisher lends slots: values written straight
 * into shared memory, which a `LOAN` record of the ring names (kind 2:
 * little-endian `u32` slot, `u32` length, `u64` generation).
 */
parcelable Pool {
    /**
     * One memfd, sealed as the topic memory is, that the subscriber maps
     * read-only: magic `RSTL` at 0, the layout version at 4, `slots` at 8,
     * `slotBytes` at 12, each slot's 64-bit generation from 0x40, then the
     * slots from `0x40 + 8 * slots` rounded up to 64, `slotBytes` each. Its
     * size is exactly that. A generation is even once the slot holds the
     * value its record names, odd while the publisher writes it.
     */
    ParcelFileDescriptor memory;

    /** Slots in the pool. */
    int slots;

    /** Bytes of a slot: a multiple of 64. */
    int slotBytes;
}

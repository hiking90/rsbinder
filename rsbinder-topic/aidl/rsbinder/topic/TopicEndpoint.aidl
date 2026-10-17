/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import android.hardware.common.fmq.MQDescriptor;
import android.hardware.common.fmq.UnsynchronizedWrite;
import rsbinder.topic.Pool;

/**
 * What a publisher returns for an accepted subscription: the topic memory to
 * map and where in it the subscription starts.
 */
parcelable TopicEndpoint {
    /** The publisher's layout version; the subscriber takes only its own. */
    int version;

    /**
     * The topic memory: one memfd, sealed with `F_SEAL_GROW`,
     * `F_SEAL_SHRINK`, `F_SEAL_FUTURE_WRITE` and `F_SEAL_SEAL`, that the
     * subscriber maps read-only. Its three grantors are where libfmq's
     * unsynchronized queue of bytes keeps them: the write region end at
     * offset 0, the write counter at 8 and the ring at 0x40, all in fd 0.
     * The ring holds records, not single bytes: a 4-byte word with the kind
     * in its top two bits and the payload length in the rest, a 4-byte
     * record index, the payload, and zeros to the next multiple of 8. A
     * libfmq reader can read its bytes but not tell where a record that
     * was overwritten while it read began.
     */
    MQDescriptor<byte, UnsynchronizedWrite> ring;

    /**
     * Where the topic header starts in the fd: magic, version, the word a
     * subscriber waits on, and the positions of the oldest, the newest and
     * the middle record. 0x10 in this version.
     */
    int headerOffset;

    /** The write counter when the subscription was taken. */
    long start;

    /** The index of the record written at `start`. */
    int startIndex;

    /**
     * The pool of a publisher that lends slots, null for one that does not.
     * A subscriber that gets one reads it with the pin word of its control
     * page.
     */
    @nullable Pool pool;
}

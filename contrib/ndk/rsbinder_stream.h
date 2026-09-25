/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * rsbinder_stream.h — the kernel-binder half of rsbinder's streaming
 * contract (rsbinder::stream::{Sink, Receiver}) for an NDK peer, as one C11
 * header on top of rsbinder-fmq/c/rsbinder_fmq.h.
 *
 * A stream over kernel binder is a ring the consumer makes and the producer
 * writes records into; rsbinder.stream.StreamEndpoint (rsbinder/aidl/stream)
 * carries its MQDescriptor to the producer in the one twoway call that opens
 * the stream, together with the consumer's sink binder. This header is the
 * record layer on that ring:
 *
 *   record   = u32 LE header, then its payload
 *   header   = kind in the top bit (0 item, 1 end), payload length below it
 *   item     = one value in parcel encoding (an `int` is 4 bytes LE)
 *   end      = i32 exception, i32 serviceSpecific (LE), then a UTF-8
 *              message, absent when the payload is exactly 8 bytes
 *
 * The producer commits each record whole and keeps the ring's last 256
 * bytes (RSBS_END_RESERVE) free of items, so the end record always fits
 * and ending never waits for the consumer. Nothing follows the end record.
 *
 * EventFlag bits: NOT_FULL (1), set by the consumer after each read and
 * waited on by the producer; NOT_EMPTY (2), set by the producer after each
 * write and waited on by the consumer; CANCEL (4), set by the consumer and
 * waited on — so consumed — by the producer alone. A libfmq producer must
 * wait on NOT_FULL | CANCEL itself instead of calling writeBlocking, which
 * treats every wake as a retry and never sees the cancel.
 *
 * Death: each end links to the other's binder — the producer to the
 * endpoint's sink, the consumer to a binder in the producer's process (the
 * service it called, or the binder an upload passed it). Nothing else wakes
 * a waiter whose peer is gone. Link with the two callbacks here, and the
 * cookie the stream hands out:
 *
 *   AIBinder_DeathRecipient *r = AIBinder_DeathRecipient_new(rsbs_on_binder_died);
 *   AIBinder_DeathRecipient_setOnUnlinked(r, rsbs_on_unlinked);   // API 33
 *   AIBinder_linkToDeath(binder, r, rsbs_producer_death_cookie(&p));
 *
 * The cookie holds its own reference on what the death callback touches
 * (the EventFlag word, mapped apart from the queue, and a dead flag), and
 * rsbs_on_unlinked drops it. libbinder may still be running the death
 * callback after AIBinder_unlinkToDeath returns; onUnlinked comes only after
 * it, and also when the link fails. So the stream may be closed at any
 * time, before or after unlinking. A cookie linked without onUnlinked is
 * never released.
 *
 * Closing mirrors rsbinder's Drop: a consumer that closes cancels the
 * producer; a producer that closes without having ended writes an
 * EX_ILLEGAL_STATE end record, so the consumer does not wait for one.
 *
 * Return codes, beyond rsbinder_fmq.h's:
 *
 *   -EPIPE      the peer's process is gone
 *   -ECANCELED  producer: the consumer wants no more
 *   -EMSGSIZE   producer: the item can never fit this ring
 *   -ENOBUFS    consumer: the caller's buffer is smaller than the record
 *   -ENODATA    consumer: the stream has already ended
 *   -ENOMEM     no memory for the death watch
 */

#ifndef RSBINDER_STREAM_H
#define RSBINDER_STREAM_H

#include "rsbinder_fmq.h"

#include <stdlib.h>

#ifdef __cplusplus
extern "C" {
#endif

#define RSBS_CANCEL 0x04u
#define RSBS_HEADER 4u
#define RSBS_KIND_END 0x80000000u
#define RSBS_LEN_MASK 0x7fffffffu
#define RSBS_END_RESERVE 256u
#define RSBS_END_FIELDS 8u
#define RSBS_END_MESSAGE_MAX (RSBS_END_RESERVE - RSBS_HEADER - RSBS_END_FIELDS)

/* The exception codes an end record carries (binder Status). */
#define RSBS_EX_NONE 0
#define RSBS_EX_ILLEGAL_ARGUMENT (-3)
#define RSBS_EX_ILLEGAL_STATE (-5)
#define RSBS_EX_SERVICE_SPECIFIC (-8)

/*
 * What a death callback touches, owned by the stream and by every live
 * death link: the dead flag, and the EventFlag word in a mapping of its
 * own, moved out of the queue so closing the queue leaves it mapped.
 */
typedef struct rsbs_watch {
    int refs;
    int dead;
    uint32_t wake;
    uint32_t *flag;
    rsbfmq_mapping flag_map;
} rsbs_watch;

typedef struct rsbs_producer {
    rsbfmq_queue q;
    rsbs_watch *watch;
    int canceled;
    int ended;
} rsbs_producer;

typedef struct rsbs_consumer {
    rsbfmq_queue q;
    rsbs_watch *watch;
    int ended;
} rsbs_consumer;

static inline void rsbs__put_u32(uint8_t *p, uint32_t x) {
    p[0] = (uint8_t)x;
    p[1] = (uint8_t)(x >> 8);
    p[2] = (uint8_t)(x >> 16);
    p[3] = (uint8_t)(x >> 24);
}

static inline uint32_t rsbs__get_u32(const uint8_t *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

/* ------------------------------------------------------------------ */
/* Death                                                               */
/* ------------------------------------------------------------------ */

/* Take the queue's EventFlag mapping into a watch that wakes `wake` on death. */
static inline int rsbs__watch(rsbfmq_queue *q, uint32_t wake, rsbs_watch **out) {
    rsbs_watch *w = (rsbs_watch *)calloc(1, sizeof *w);
    if (!w) {
        return -ENOMEM;
    }
    w->refs = 1;
    w->wake = wake;
    w->flag = q->flag;
    w->flag_map = q->maps[RSBFMQ_EVENT_FLAG_WORD];
    /* The queue still reads q->flag; the watch, which outlives it, unmaps. */
    q->maps[RSBFMQ_EVENT_FLAG_WORD].base = NULL;
    q->maps[RSBFMQ_EVENT_FLAG_WORD].len = 0;
    *out = w;
    return 0;
}

static inline void rsbs__release(rsbs_watch *w) {
    if (w && __atomic_sub_fetch(&w->refs, 1, __ATOMIC_ACQ_REL) == 0) {
        munmap(w->flag_map.base, w->flag_map.len);
        free(w);
    }
}

static inline void *rsbs__cookie(rsbs_watch *w) {
    __atomic_add_fetch(&w->refs, 1, __ATOMIC_RELAXED);
    return w;
}

/* AIBinder_DeathRecipient_onBinderDied: mark the peer dead, wake this side's waiter. */
static inline void rsbs_on_binder_died(void *cookie) {
    rsbs_watch *w = (rsbs_watch *)cookie;
    __atomic_store_n(&w->dead, 1, __ATOMIC_SEQ_CST);
    rsbfmq_word_wake(w->flag, w->wake);
}

/* AIBinder_DeathRecipient_onBinderUnlinked: drop the link's reference. */
static inline void rsbs_on_unlinked(void *cookie) {
    rsbs__release((rsbs_watch *)cookie);
}

/* ------------------------------------------------------------------ */
/* Producer                                                            */
/* ------------------------------------------------------------------ */

/*
 * Attach to the ring an endpoint carries. `max_ring_bytes` bounds what this
 * side maps (rsbinder's default is 4 MiB). The ring must be sealed (or
 * ashmem), carry an EventFlag word, and leave room for an item past the
 * end reserve.
 */
static inline int rsbs_producer_attach(rsbs_producer *p, const rsbfmq_desc *ring,
                                       size_t max_ring_bytes) {
    rsbfmq_policy policy;
    int r;
    memset(p, 0, sizeof *p);
    policy.max_capacity = max_ring_bytes;
    policy.require_seal = 1;
    policy.require_event_flag = 1;
    r = rsbfmq_attach(&p->q, ring, 1, &policy);
    if (r == 0 && p->q.capacity <= RSBS_END_RESERVE + RSBS_HEADER) {
        r = -EINVAL;
    }
    if (r == 0) {
        /* Its own mask: the death wakes a send parked for room. */
        r = rsbs__watch(&p->q, RSBFMQ_NOT_FULL, &p->watch);
    }
    if (r != 0) {
        rsbfmq_close(&p->q);
    }
    return r;
}

/* The cookie for AIBinder_linkToDeath on the endpoint's sink; see the top of this file. */
static inline void *rsbs_producer_death_cookie(rsbs_producer *p) {
    return rsbs__cookie(p->watch);
}

/* The largest item this ring takes. */
static inline size_t rsbs_max_item(const rsbs_producer *p) {
    return p->q.capacity - RSBS_END_RESERVE - RSBS_HEADER;
}

/* Write one record within `limit` bytes in flight; `blocking` waits for room. */
static inline int rsbs__write(rsbs_producer *p, uint32_t header, const void *payload,
                              size_t len, size_t limit, int blocking) {
    size_t n = RSBS_HEADER + len;
    int is_end = (header & RSBS_KIND_END) != 0;
    for (;;) {
        size_t in_flight;
        uint32_t set = 0;
        int r;
        if (__atomic_load_n(&p->watch->dead, __ATOMIC_SEQ_CST)) {
            return -EPIPE;
        }
        if (!is_end && (p->canceled || (rsbfmq_peek(&p->q) & RSBS_CANCEL) != 0)) {
            p->canceled = 1;
            return -ECANCELED;
        }
        if ((r = rsbfmq_available_to_read(&p->q, &in_flight)) != 0) {
            return r;
        }
        if (in_flight + n <= limit) {
            rsbfmq_regions regions;
            uint8_t h[RSBS_HEADER];
            r = rsbfmq_begin_write(&p->q, n, &regions);
            if (r == -EAGAIN) {
                /* The counters said it fits and now say it does not: the peer moved them. */
                return -EBADMSG;
            }
            if (r != 0) {
                return r;
            }
            rsbs__put_u32(h, header);
            rsbfmq_regions_write(&regions, 0, h, RSBS_HEADER);
            rsbfmq_regions_write(&regions, RSBS_HEADER, payload, len);
            if ((r = rsbfmq_commit_write(&p->q, n)) != 0) {
                return r;
            }
            return rsbfmq_wake(&p->q, RSBFMQ_NOT_EMPTY);
        }
        if (!blocking) {
            return -EAGAIN;
        }
        if ((r = rsbfmq_wait_until(&p->q, RSBFMQ_NOT_FULL | RSBS_CANCEL, NULL, &set)) != 0) {
            return r;
        }
        if ((set & RSBS_CANCEL) != 0 && !is_end) {
            p->canceled = 1;
            return -ECANCELED;
        }
    }
}

/*
 * Send one item: `len` bytes of parcel-encoded value. With `blocking` set,
 * waits while the ring is full; otherwise -EAGAIN. An item larger than
 * rsbs_max_item is -EMSGSIZE and the stream goes on.
 */
static inline int rsbs_send(rsbs_producer *p, const void *item, size_t len, int blocking) {
    if (p->ended) {
        return -EINVAL;
    }
    if (len > rsbs_max_item(p)) {
        return -EMSGSIZE;
    }
    return rsbs__write(p, (uint32_t)len, item, len, p->q.capacity - RSBS_END_RESERVE, blocking);
}

/*
 * End the stream: RSBS_EX_NONE for a clean end, or the status the stream
 * failed with. `message` may be NULL; it is cut to RSBS_END_MESSAGE_MAX
 * bytes on a UTF-8 character boundary. Never waits: the reserve holds it.
 * A cancel does not stop it; a dead consumer does (-EPIPE).
 */
static inline int rsbs_end(rsbs_producer *p, int32_t exception, int32_t service_specific,
                           const char *message) {
    uint8_t payload[RSBS_END_RESERVE - RSBS_HEADER];
    size_t len = RSBS_END_FIELDS;
    int r;
    if (p->ended) {
        return -EINVAL;
    }
    rsbs__put_u32(payload, (uint32_t)exception);
    rsbs__put_u32(payload + 4, (uint32_t)service_specific);
    if (message) {
        size_t m = strlen(message);
        if (m > RSBS_END_MESSAGE_MAX) {
            m = RSBS_END_MESSAGE_MAX;
            while (m > 0 && ((unsigned char)message[m] & 0xC0) == 0x80) {
                m--;
            }
        }
        memcpy(payload + RSBS_END_FIELDS, message, m);
        len += m;
    }
    r = rsbs__write(p, RSBS_KIND_END | (uint32_t)len, payload, len, p->q.capacity, 0);
    /* Items never pass the reserve, so no room means the consumer moved the counters. */
    if (r == -EAGAIN) {
        r = -EBADMSG;
    }
    if (r == 0) {
        p->ended = 1;
    }
    return r;
}

/* Whether the consumer has cancelled, as far as this side has seen. */
static inline int rsbs_producer_canceled(rsbs_producer *p) {
    if ((rsbfmq_peek(&p->q) & RSBS_CANCEL) != 0) {
        p->canceled = 1;
    }
    return p->canceled;
}

/*
 * Close the stream. One that was not ended gets an EX_ILLEGAL_STATE end
 * record first, as rsbinder's Drop for Sink writes, so the consumer is not
 * left waiting. Safe on a zeroed or failed-to-attach producer.
 */
static inline void rsbs_producer_close(rsbs_producer *p) {
    if (p->watch && !p->ended && !__atomic_load_n(&p->watch->dead, __ATOMIC_SEQ_CST)) {
        rsbs_end(p, RSBS_EX_ILLEGAL_STATE, 0,
                 "the producer closed the stream without ending it");
    }
    rsbfmq_close(&p->q);
    rsbs__release(p->watch);
    p->watch = NULL;
}

/* ------------------------------------------------------------------ */
/* Consumer                                                            */
/* ------------------------------------------------------------------ */

/*
 * Make the ring: `ring_bytes` of shared memory charged to this process, in
 * which the producer can have at most `ring_bytes - 256` bytes of items in
 * flight. rsbinder's default is 64 KiB; an item larger than
 * `ring_bytes - 260` cannot be sent into it.
 */
static inline int rsbs_consumer_create(rsbs_consumer *c, size_t ring_bytes) {
    int r;
    memset(c, 0, sizeof *c);
    if (ring_bytes <= RSBS_END_RESERVE + RSBS_HEADER) {
        c->q.fd = -1;
        return -EINVAL;
    }
    r = rsbfmq_create(&c->q, ring_bytes, 1, 1);
    if (r == 0) {
        /* Its own mask: the death wakes a recv parked on an empty ring. */
        r = rsbs__watch(&c->q, RSBFMQ_NOT_EMPTY, &c->watch);
        if (r != 0) {
            rsbfmq_close(&c->q);
        }
    }
    return r;
}

/* The cookie for AIBinder_linkToDeath on the producer's binder; see the top of this file. */
static inline void *rsbs_consumer_death_cookie(rsbs_consumer *c) {
    return rsbs__cookie(c->watch);
}

/* A buffer this size holds any record rsbs_recv can return. */
static inline size_t rsbs_max_record(const rsbs_consumer *c) {
    return c->q.capacity - RSBS_HEADER;
}

/* The ring is broken: stop the producer, end the stream here. */
static inline int rsbs__corrupted(rsbs_consumer *c) {
    c->ended = 1;
    rsbfmq_wake(&c->q, RSBS_CANCEL);
    return -EBADMSG;
}

/*
 * Take the next record into `buf`: its payload length in `*len`, and
 * `*is_end` set when it is the end record (parse it with rsbs_end_fields).
 * Waits up to `timeout_ns` for one (negative: without a deadline, 0: not at
 * all, -EAGAIN). Records the producer wrote before it died are still
 * returned; -EPIPE comes once none are left. A record that breaks the
 * contract ends the stream with -EBADMSG and cancels the producer.
 */
static inline int rsbs_recv(rsbs_consumer *c, void *buf, size_t cap, size_t *len, int *is_end,
                            int64_t timeout_ns) {
    struct timespec deadline;
    const struct timespec *until = NULL;
    if (c->ended) {
        return -ENODATA;
    }
    if (timeout_ns > 0) {
        int r = rsbfmq_deadline_after(timeout_ns, &deadline);
        if (r < 0) {
            return r;
        }
        until = r == 0 ? &deadline : NULL;
    }
    for (;;) {
        size_t available, most, record;
        uint8_t h[RSBS_HEADER];
        uint32_t header, set = 0;
        rsbfmq_regions regions;
        int r, end;
        /* Before the look: a death noticed after it may follow a record the look missed. */
        int dead = __atomic_load_n(&c->watch->dead, __ATOMIC_SEQ_CST);
        if ((r = rsbfmq_available_to_read(&c->q, &available)) != 0) {
            return rsbs__corrupted(c);
        }
        if (available == 0) {
            if (dead) {
                c->ended = 1;
                return -EPIPE;
            }
            if (timeout_ns == 0) {
                return -EAGAIN;
            }
            /* A death since the look woke NOT_EMPTY, so this returns at once. */
            if ((r = rsbfmq_wait_until(&c->q, RSBFMQ_NOT_EMPTY, until, &set)) != 0) {
                return r;
            }
            continue;
        }
        if (available < RSBS_HEADER || rsbfmq_begin_read(&c->q, RSBS_HEADER, &regions) != 0) {
            return rsbs__corrupted(c);
        }
        rsbfmq_regions_read(&regions, 0, h, RSBS_HEADER);
        header = rsbs__get_u32(h);
        end = (header & RSBS_KIND_END) != 0;
        record = header & RSBS_LEN_MASK;
        most = end ? c->q.capacity - RSBS_HEADER
                   : c->q.capacity - RSBS_END_RESERVE - RSBS_HEADER;
        /* Committed whole, so the bytes are there or the counters lie. */
        if (record > most || (end && record < RSBS_END_FIELDS) ||
            available < RSBS_HEADER + record) {
            return rsbs__corrupted(c);
        }
        if (record > cap) {
            return -ENOBUFS;
        }
        if (rsbfmq_begin_read(&c->q, RSBS_HEADER + record, &regions) != 0) {
            return rsbs__corrupted(c);
        }
        rsbfmq_regions_read(&regions, RSBS_HEADER, buf, record);
        if (rsbfmq_commit_read(&c->q, RSBS_HEADER + record) != 0) {
            return rsbs__corrupted(c);
        }
        /* Every read wakes, as libfmq does: a producer may wait for room below full. */
        rsbfmq_wake(&c->q, RSBFMQ_NOT_FULL);
        if (end) {
            c->ended = 1;
        }
        *len = record;
        *is_end = end;
        return 0;
    }
}

/*
 * The three fields of an end record's payload; `*message` is not
 * NUL-terminated. -EINVAL for a payload shorter than the two fields (not
 * an end record).
 */
static inline int rsbs_end_fields(const void *payload, size_t len, int32_t *exception,
                                  int32_t *service_specific, const char **message,
                                  size_t *message_len) {
    const uint8_t *p = (const uint8_t *)payload;
    if (len < RSBS_END_FIELDS) {
        return -EINVAL;
    }
    *exception = (int32_t)rsbs__get_u32(p);
    *service_specific = (int32_t)rsbs__get_u32(p + 4);
    *message = (const char *)p + RSBS_END_FIELDS;
    *message_len = len - RSBS_END_FIELDS;
    return 0;
}

/*
 * Want no more: the producer's next send returns -ECANCELED, a parked one
 * at once. Its end record, which the cancel does not stop, can still be
 * read.
 */
static inline int rsbs_cancel(rsbs_consumer *c) {
    return rsbfmq_wake(&c->q, RSBS_CANCEL);
}

/*
 * Close the stream. Cancels first, as rsbinder's Drop for Receiver does, so
 * a producer parked on a full ring is released. Safe on a zeroed or
 * failed-to-create consumer.
 */
static inline void rsbs_consumer_close(rsbs_consumer *c) {
    if (c->watch) {
        rsbfmq_wake(&c->q, RSBS_CANCEL);
    }
    rsbfmq_close(&c->q);
    rsbs__release(c->watch);
    c->watch = NULL;
}

#ifdef __cplusplus
} /* extern "C" */

#include <utility>

/*
 * The ring of an NDK-backend rsbinder.stream.StreamEndpoint, as a view for
 * rsbs_producer_attach. -EINVAL when it has none: the consumer is on an RPC
 * session, whose stream is the IStreamSink/IStreamSource calls instead.
 */
template <class StreamEndpoint>
inline int rsbs_endpoint_ring(const StreamEndpoint &endpoint, std::vector<int> &fds,
                              std::vector<rsbfmq_grantor> &grantors, rsbfmq_desc *out) {
    if (!endpoint.ring) {
        return -EINVAL;
    }
    return rsbfmq_desc_from_aidl(*endpoint.ring, fds, grantors, out);
}

/* The endpoint a consumer hands its producer: its ring and a binder of its own. */
template <class StreamEndpoint, class Binder>
inline int rsbs_endpoint_make(const rsbs_consumer &c, Binder sink, StreamEndpoint *out) {
    out->ring.emplace();
    out->sink = std::move(sink);
    return rsbfmq_desc_to_aidl(c.q, &*out->ring);
}
#endif /* __cplusplus */

#endif /* RSBINDER_STREAM_H */

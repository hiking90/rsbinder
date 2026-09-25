/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * contrib/ndk/rsbinder_stream.h, both ends in one process: what the NDK
 * STAGE3 (example-hello/cpp/run_stream_interop.sh) cannot pin down by
 * timing — the death watch's lifetime, what closing does to the other end,
 * the order of a death and the records before it. Built with
 * AddressSanitizer by tests/tests/c_stream_header.rs; prints "ok".
 */

#ifndef _GNU_SOURCE /* g++ defines it */
#define _GNU_SOURCE
#endif
#include "rsbinder_stream.h"

#include <pthread.h>

#define CHECK(cond)                                                              \
    do {                                                                         \
        if (!(cond)) {                                                           \
            fprintf(stderr, "%s:%d: check failed: %s\n", __FILE__, __LINE__, #cond); \
            exit(1);                                                             \
        }                                                                        \
    } while (0)

#define RING 4100 /* not a multiple of the 8-byte record: records cross the end */
#define SECOND (1000LL * 1000 * 1000)

static void pair(rsbs_consumer *c, rsbs_producer *p) {
    rsbfmq_desc d;
    CHECK(rsbs_consumer_create(c, RING) == 0);
    CHECK(rsbfmq_descriptor(&c->q, &d) == 0);
    CHECK(rsbs_producer_attach(p, &d, 1 << 20) == 0);
}

static int recv_item(rsbs_consumer *c, int32_t *item, int *is_end, int64_t timeout) {
    uint8_t buf[RING];
    size_t len;
    int r = rsbs_recv(c, buf, sizeof buf, &len, is_end, timeout);
    if (r == 0 && !*is_end) {
        CHECK(len == 4);
        memcpy(item, buf, 4);
    }
    return r;
}

struct sender {
    rsbs_producer *p;
    int count;
    int rc;
};

static void *send_all(void *arg) {
    struct sender *s = (struct sender *)arg;
    int32_t i;
    s->rc = 0;
    for (i = 0; i < s->count && s->rc == 0; i++) {
        s->rc = rsbs_send(s->p, &i, 4, 1);
    }
    if (s->rc == 0) {
        s->rc = rsbs_end(s->p, RSBS_EX_NONE, 0, NULL);
    }
    return NULL;
}

/* Many laps of the ring with both ends running; the end record comes last. */
static void records_cross_in_order(void) {
    rsbs_consumer c;
    rsbs_producer p;
    struct sender s;
    pthread_t t;
    int32_t item, expected = 0;
    int is_end = 0;
    pair(&c, &p);
    s.p = &p;
    s.count = 20000;
    pthread_create(&t, NULL, send_all, &s);
    while (recv_item(&c, &item, &is_end, 10 * SECOND) == 0 && !is_end) {
        CHECK(item == expected);
        expected++;
    }
    pthread_join(t, NULL);
    CHECK(is_end && expected == 20000 && s.rc == 0);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);
}

/* A consumer that closes releases a producer parked on a full ring (Drop for Receiver). */
static void consumer_close_cancels(void) {
    rsbs_consumer c;
    rsbs_producer p;
    struct sender s;
    pthread_t t;
    size_t used;
    pair(&c, &p);
    s.p = &p;
    s.count = 1 << 30;
    pthread_create(&t, NULL, send_all, &s);
    do {
        usleep(1000);
        CHECK(rsbfmq_available_to_read(&c.q, &used) == 0);
    } while (used + 8 <= RING - RSBS_END_RESERVE);
    usleep(20000); /* parked on the full ring by now */
    rsbs_consumer_close(&c);
    pthread_join(t, NULL);
    CHECK(s.rc == -ECANCELED);
    rsbs_producer_close(&p);
}

/* A producer that closes without ending leaves an EX_ILLEGAL_STATE end (Drop for Sink). */
static void producer_close_ends(void) {
    rsbs_consumer c;
    rsbs_producer p;
    uint8_t buf[RING];
    size_t len;
    int32_t item = 7, exception, code;
    const char *message;
    size_t message_len;
    int is_end;
    pair(&c, &p);
    CHECK(rsbs_send(&p, &item, 4, 1) == 0);
    rsbs_producer_close(&p);
    CHECK(recv_item(&c, &item, &is_end, 0) == 0 && !is_end && item == 7);
    CHECK(rsbs_recv(&c, buf, sizeof buf, &len, &is_end, 0) == 0 && is_end);
    CHECK(rsbs_end_fields(buf, len, &exception, &code, &message, &message_len) == 0);
    CHECK(exception == RSBS_EX_ILLEGAL_STATE && message_len > 0);
    CHECK(rsbs_end_fields(buf, 7, &exception, &code, &message, &message_len) == -EINVAL);
    rsbs_consumer_close(&c);
}

/* A cancel stops items, not the end record, and the consumer can still read it. */
static void cancel_leaves_the_end_readable(void) {
    rsbs_consumer c;
    rsbs_producer p;
    int32_t item = 1;
    int is_end;
    pair(&c, &p);
    CHECK(rsbs_cancel(&c) == 0);
    CHECK(rsbs_send(&p, &item, 4, 1) == -ECANCELED);
    CHECK(rsbs_end(&p, RSBS_EX_NONE, 0, NULL) == 0);
    CHECK(recv_item(&c, &item, &is_end, 0) == 0 && is_end);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);
}

/* Records written before the producer died still come out; then -EPIPE. */
static void death_after_records(void) {
    rsbs_consumer c;
    rsbs_producer p;
    int32_t item = 3;
    int is_end;
    void *cookie;
    pair(&c, &p);
    cookie = rsbs_consumer_death_cookie(&c);
    CHECK(rsbs_send(&p, &item, 4, 1) == 0);
    rsbs_on_binder_died(cookie);
    CHECK(recv_item(&c, &item, &is_end, 0) == 0 && item == 3);
    CHECK(recv_item(&c, &item, &is_end, 0) == -EPIPE);
    rsbs_on_unlinked(cookie);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);
}

struct death {
    void *cookie;
};

static void *die_soon(void *arg) {
    usleep(50000);
    rsbs_on_binder_died(((struct death *)arg)->cookie);
    return NULL;
}

/* Each side's death wakes its own parked waiter. */
static void death_wakes_the_waiter(void) {
    rsbs_consumer c;
    rsbs_producer p;
    struct death d;
    struct sender s;
    pthread_t t, killer;
    int32_t item;
    int is_end;

    pair(&c, &p);
    d.cookie = rsbs_consumer_death_cookie(&c);
    pthread_create(&killer, NULL, die_soon, &d);
    CHECK(recv_item(&c, &item, &is_end, -1) == -EPIPE); /* parked on an empty ring */
    pthread_join(killer, NULL);
    rsbs_on_unlinked(d.cookie);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);

    pair(&c, &p);
    d.cookie = rsbs_producer_death_cookie(&p);
    s.p = &p;
    s.count = 1 << 30;
    pthread_create(&killer, NULL, die_soon, &d);
    pthread_create(&t, NULL, send_all, &s); /* fills the ring, parks */
    pthread_join(t, NULL);
    pthread_join(killer, NULL);
    CHECK(s.rc == -EPIPE);
    rsbs_on_unlinked(d.cookie);
    rsbs_producer_close(&p); /* dead: no end record, and nothing waits */
    rsbs_consumer_close(&c);
}

/* The death callback may run after the stream closed; the cookie keeps what it touches. */
static void death_after_close(void) {
    rsbs_consumer c;
    rsbs_producer p;
    void *consumer_cookie, *producer_cookie;
    pair(&c, &p);
    consumer_cookie = rsbs_consumer_death_cookie(&c);
    producer_cookie = rsbs_producer_death_cookie(&p);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);
    rsbs_on_binder_died(consumer_cookie);
    rsbs_on_binder_died(producer_cookie);
    rsbs_on_unlinked(consumer_cookie);
    rsbs_on_unlinked(producer_cookie);
}

/* Closing what never opened, or twice, is harmless. */
static void close_is_idempotent(void) {
    rsbs_consumer c;
    rsbs_producer p;
    memset(&c, 0, sizeof c);
    memset(&p, 0, sizeof p);
    rsbs_consumer_close(&c);
    rsbs_producer_close(&p);
    CHECK(rsbs_consumer_create(&c, RSBS_END_RESERVE) == -EINVAL);
    rsbs_consumer_close(&c);
    pair(&c, &p);
    rsbs_producer_close(&p);
    rsbs_producer_close(&p);
    rsbs_consumer_close(&c);
    rsbs_consumer_close(&c);
}

int main(void) {
    records_cross_in_order();
    consumer_close_cancels();
    producer_close_ends();
    cancel_leaves_the_end_readable();
    death_after_records();
    death_wakes_the_waiter();
    death_after_close();
    close_is_idempotent();
    printf("ok\n");
    return 0;
}

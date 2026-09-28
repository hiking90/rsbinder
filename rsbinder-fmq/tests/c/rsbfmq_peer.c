/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * The other end of tests/c_header.rs: c/rsbinder_fmq.h driven the way
 * tests/cpp/libfmq_peer.cpp drives libfmq, over the same descriptor record
 * (tests/common/mod.rs) and the same one-byte acks.
 *
 *   rsbfmq_peer <create|attach> <read|write> <socket> <count> <capacity>
 *   rsbfmq_peer probe <socket> <max_capacity>
 *
 * `probe` attaches whatever descriptor arrives and prints the result code,
 * then, once the test has damaged the counters, what reading reports.
 */

#define _GNU_SOURCE
#include "rsbinder_fmq.h"

#include <stdlib.h>
#include <sys/socket.h>
#include <sys/un.h>

#define TIMEOUT_NS (10LL * 1000 * 1000 * 1000)

static void die(const char *what) {
    perror(what);
    exit(2);
}

static int connect_unix(const char *path) {
    struct sockaddr_un addr;
    int i, fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) {
        die("socket");
    }
    memset(&addr, 0, sizeof addr);
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, path, sizeof addr.sun_path - 1);
    for (i = 0; i < 2000; i++) {
        if (connect(fd, (struct sockaddr *)&addr, sizeof addr) == 0) {
            return fd;
        }
        usleep(5000);
    }
    die("connect");
    return -1;
}

static void put_u32(uint8_t *p, uint32_t x) {
    int i;
    for (i = 0; i < 4; i++) {
        p[i] = (uint8_t)(x >> (8 * i));
    }
}

static uint32_t get_u32(const uint8_t *p) {
    return (uint32_t)p[0] | (uint32_t)p[1] << 8 | (uint32_t)p[2] << 16 | (uint32_t)p[3] << 24;
}

static void send_descriptor(int sock, const rsbfmq_desc *d) {
    uint8_t msg[4 + 16 + 4 * 16];
    union {
        char buf[CMSG_SPACE(sizeof(int))];
        struct cmsghdr align;
    } control;
    struct iovec iov;
    struct msghdr mh;
    struct cmsghdr *c;
    size_t i, o = 20;
    put_u32(msg + 4, d->quantum);
    put_u32(msg + 8, d->flags);
    put_u32(msg + 12, (uint32_t)d->ngrantors);
    put_u32(msg + 16, 0);
    for (i = 0; i < d->ngrantors; i++) {
        put_u32(msg + o, d->grantors[i].fd_index);
        put_u32(msg + o + 4, d->grantors[i].offset);
        put_u32(msg + o + 8, (uint32_t)d->grantors[i].extent);
        put_u32(msg + o + 12, (uint32_t)(d->grantors[i].extent >> 32));
        o += 16;
    }
    put_u32(msg, (uint32_t)(o - 4));
    iov.iov_base = msg;
    iov.iov_len = o;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = control.buf;
    mh.msg_controllen = sizeof control.buf;
    c = CMSG_FIRSTHDR(&mh);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int));
    memcpy(CMSG_DATA(c), d->fds, sizeof(int));
    if (sendmsg(sock, &mh, 0) != (ssize_t)o) {
        die("sendmsg");
    }
}

/* Fills the arrays `d` points at; fds are received as SCM_RIGHTS. */
static void recv_descriptor(int sock, rsbfmq_desc *d, int *fds, rsbfmq_grantor *grantors) {
    uint8_t data[4096];
    union {
        char buf[CMSG_SPACE(sizeof(int) * 8)];
        struct cmsghdr align;
    } control;
    struct iovec iov;
    struct msghdr mh;
    struct cmsghdr *c;
    size_t nfds = 0, i, o = 20;
    ssize_t n;
    uint32_t ngrantors;
    iov.iov_base = data;
    iov.iov_len = sizeof data;
    memset(&mh, 0, sizeof mh);
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = control.buf;
    mh.msg_controllen = sizeof control.buf;
    n = recvmsg(sock, &mh, 0);
    if (n < 20) {
        die("recvmsg");
    }
    for (c = CMSG_FIRSTHDR(&mh); c; c = CMSG_NXTHDR(&mh, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            size_t count = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            for (i = 0; i < count && nfds < 8; i++) {
                memcpy(&fds[nfds++], CMSG_DATA(c) + i * sizeof(int), sizeof(int));
            }
        }
    }
    if (get_u32(data) + 4 != (uint32_t)n) {
        fprintf(stderr, "record length\n");
        exit(2);
    }
    ngrantors = get_u32(data + 12);
    if (ngrantors > 8) {
        fprintf(stderr, "too many grantors\n");
        exit(2);
    }
    for (i = 0; i < ngrantors; i++) {
        grantors[i].fd_index = get_u32(data + o);
        grantors[i].offset = get_u32(data + o + 4);
        grantors[i].extent = (uint64_t)get_u32(data + o + 8) | (uint64_t)get_u32(data + o + 12) << 32;
        o += 16;
    }
    d->fds = fds;
    d->nfds = nfds;
    d->grantors = grantors;
    d->ngrantors = ngrantors;
    d->quantum = get_u32(data + 4);
    d->flags = get_u32(data + 8);
}

static void send_byte(int sock) {
    uint8_t b = 1;
    if (write(sock, &b, 1) != 1) {
        die("write");
    }
}

static void recv_byte(int sock) {
    uint8_t b;
    if (read(sock, &b, 1) != 1) {
        die("read");
    }
}

/* libfmq writeBlocking: wait on NOT_FULL for room, wake NOT_EMPTY after each commit. */
static void run_writer(rsbfmq_queue *q, uint32_t count) {
    uint32_t next = 0, chunk[5], set;
    while (next < count) {
        size_t i, n = count - next < 5 ? count - next : 5;
        int r;
        for (i = 0; i < n; i++) {
            chunk[i] = next + (uint32_t)i;
        }
        while ((r = rsbfmq_write(q, chunk, n)) == -EAGAIN) {
            if ((r = rsbfmq_wait(q, RSBFMQ_NOT_FULL, TIMEOUT_NS, &set)) != 0) {
                fprintf(stderr, "wait NOT_FULL at %u: %d\n", next, r);
                exit(1);
            }
        }
        if (r != 0 || rsbfmq_wake(q, RSBFMQ_NOT_EMPTY) != 0) {
            fprintf(stderr, "write at %u: %d\n", next, r);
            exit(1);
        }
        next += (uint32_t)n;
    }
}

static void run_reader(rsbfmq_queue *q, uint32_t count) {
    uint32_t next = 0, chunk[7], set;
    unsigned long long sum = 0;
    while (next < count) {
        size_t i, n = count - next < 7 ? count - next : 7;
        int r;
        while ((r = rsbfmq_read(q, chunk, n)) == -EAGAIN) {
            if ((r = rsbfmq_wait(q, RSBFMQ_NOT_EMPTY, TIMEOUT_NS, &set)) != 0) {
                fprintf(stderr, "wait NOT_EMPTY at %u: %d\n", next, r);
                exit(1);
            }
        }
        if (r != 0 || rsbfmq_wake(q, RSBFMQ_NOT_FULL) != 0) {
            fprintf(stderr, "read at %u: %d\n", next, r);
            exit(1);
        }
        for (i = 0; i < n; i++) {
            if (chunk[i] != next) {
                fprintf(stderr, "item %u: got %u\n", next, chunk[i]);
                exit(1);
            }
            sum += chunk[i];
            next++;
        }
    }
    printf("sum=%llu\n", sum);
    fflush(stdout);
}

static int probe(int sock, size_t max_capacity) {
    int fds[8];
    rsbfmq_grantor grantors[8];
    rsbfmq_desc d;
    rsbfmq_policy p;
    rsbfmq_queue q;
    rsbfmq_regions regions;
    size_t n;
    int r;
    recv_descriptor(sock, &d, fds, grantors);
    p.max_capacity = max_capacity;
    p.require_seal = 1;
    p.require_event_flag = 1;
    r = rsbfmq_attach(&q, &d, 4, &p);
    printf("attach=%d\n", r);
    fflush(stdout);
    send_byte(sock);
    if (r != 0) {
        return 0;
    }
    recv_byte(sock); /* the counters have been damaged */
    printf("available=%d begin_read=%d begin_write=%d\n", rsbfmq_available_to_read(&q, &n),
           rsbfmq_begin_read(&q, 1, &regions), rsbfmq_begin_write(&q, 1, &regions));
    fflush(stdout);
    rsbfmq_close(&q);
    return 0;
}

int main(int argc, char **argv) {
    int fds[8], sock, r;
    rsbfmq_grantor grantors[8];
    rsbfmq_desc d;
    rsbfmq_queue q;
    uint32_t count;
    size_t capacity;
    if (argc == 4 && strcmp(argv[1], "probe") == 0) {
        return probe(connect_unix(argv[2]), strtoul(argv[3], NULL, 10));
    }
    if (argc != 6) {
        fprintf(stderr, "usage: %s <create|attach> <read|write> <socket> <count> <capacity>\n",
                argv[0]);
        return 2;
    }
    count = (uint32_t)strtoul(argv[4], NULL, 10);
    capacity = strtoul(argv[5], NULL, 10);
    sock = connect_unix(argv[3]);

    if (strcmp(argv[1], "create") == 0) {
        if ((r = rsbfmq_create(&q, capacity, 4, 1)) != 0) {
            fprintf(stderr, "create: %d\n", r);
            return 1;
        }
        rsbfmq_descriptor(&q, &d);
        send_descriptor(sock, &d);
    } else {
        rsbfmq_policy p;
        p.max_capacity = capacity;
        p.require_seal = 1;
        p.require_event_flag = 1;
        recv_descriptor(sock, &d, fds, grantors);
        if ((r = rsbfmq_attach(&q, &d, 4, &p)) != 0) {
            fprintf(stderr, "attach: %d\n", r);
            return 1;
        }
    }
    printf("capacity=%zu grantors=%d\n", q.capacity, q.flag ? 4 : 3);
    fflush(stdout);
    send_byte(sock); /* attached, or created and described */

    if (strcmp(argv[2], "write") == 0) {
        recv_byte(sock); /* the peer has attached */
        run_writer(&q, count);
        recv_byte(sock); /* the peer has read everything */
        printf("done\n");
    } else {
        run_reader(&q, count);
    }
    rsbfmq_close(&q);
    return 0;
}

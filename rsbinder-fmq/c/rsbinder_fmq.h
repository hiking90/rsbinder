/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

/*
 * rsbinder_fmq.h — Android's Fast Message Queue (libfmq), synchronized
 * flavor, as one C11 header.
 *
 * The same queue `rsbinder-fmq` implements in Rust, for a peer that cannot
 * link libfmq: an NDK app (libfmq is not part of the NDK, and the device's
 * libfmq.so is built against the platform's libc++, not the NDK's), or a
 * plain Linux process. A queue made here attaches from rsbinder-fmq or
 * libfmq and the reverse; the layout, the counters and the EventFlag
 * protocol are libfmq's (system/libfmq: MessageQueueBase.h, EventFlag.cpp,
 * AidlMQDescriptorShimBase.h).
 *
 * Nothing here knows about binder. A queue travels as a descriptor — fds,
 * grantors, quantum, flags — which is what the AIDL type
 * android.hardware.common.fmq.MQDescriptor carries; the two C++ templates at
 * the end convert to and from the NDK backend's generated type.
 *
 * Build: C11 or C++11 and later, GCC or Clang (the `__atomic` builtins),
 * Linux or Android. On glibc define _GNU_SOURCE before the first system
 * header (or compile with -std=gnu11): fallocate and the seal constants are
 * GNU extensions there. Every function returns 0 or a negative errno:
 *
 *   -EINVAL    a descriptor or argument that breaks a rule below
 *   -EBADMSG   counters the peer moved out of their invariant
 *   -EAGAIN    not enough room or data for the whole request, right now
 *   -ETIMEDOUT a wait whose deadline passed
 *   other      the failing system call's errno
 *
 * One reader and one writer per queue, each on one thread at a time; the
 * EventFlag functions may be called from any thread (a death notification
 * waking a waiter, for instance) as long as the queue is not closed under
 * them.
 */

#ifndef RSBINDER_FMQ_H
#define RSBINDER_FMQ_H

/* First, so that on glibc the check below sees what features.h decided. */
#include <errno.h>

#if defined(__GLIBC__) && !defined(__USE_GNU)
#error "rsbinder_fmq.h: define _GNU_SOURCE before including any system header"
#endif

#include <fcntl.h>
#include <limits.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>
#include <linux/futex.h>

/* Older C library headers lack these; the values are the kernel's. */
#ifndef F_ADD_SEALS
#define F_ADD_SEALS 1033
#endif
#ifndef F_GET_SEALS
#define F_GET_SEALS 1034
#endif
#ifndef F_SEAL_SEAL
#define F_SEAL_SEAL 0x0001
#endif
#ifndef F_SEAL_SHRINK
#define F_SEAL_SHRINK 0x0002
#endif
#ifndef F_SEAL_GROW
#define F_SEAL_GROW 0x0004
#endif
#ifndef MFD_CLOEXEC
#define MFD_CLOEXEC 0x0001U
#endif
#ifndef MFD_ALLOW_SEALING
#define MFD_ALLOW_SEALING 0x0002U
#endif
#ifndef FUTEX_WAIT_BITSET
#define FUTEX_WAIT_BITSET 9
#endif
#ifndef FUTEX_WAKE_BITSET
#define FUTEX_WAKE_BITSET 10
#endif

#ifdef __cplusplus
extern "C" {
#endif

/* libfmq FMQ_NOT_FULL: the reader sets it after consuming, the writer waits on it. */
#define RSBFMQ_NOT_FULL 0x01u
/* libfmq FMQ_NOT_EMPTY: the writer sets it after producing, the reader waits on it. */
#define RSBFMQ_NOT_EMPTY 0x02u

/* MQDescriptor.flags: MQFlavor. Only the synchronized flavor attaches. */
#define RSBFMQ_SYNCHRONIZED_READ_WRITE 0x01u

/* The fixed role of each grantor index (libfmq MQDescriptorBase.h). */
#define RSBFMQ_READ_COUNTER 0
#define RSBFMQ_WRITE_COUNTER 1
#define RSBFMQ_DATA 2
#define RSBFMQ_EVENT_FLAG_WORD 3

/* One region: `extent` bytes at `offset` into fds[fd_index]. */
typedef struct rsbfmq_grantor {
    uint32_t fd_index;
    uint32_t offset;
    uint64_t extent;
} rsbfmq_grantor;

/*
 * A descriptor, as a view over arrays the caller owns: MQDescriptor's
 * handle.fds, grantors, quantum and flags. Attaching maps what it needs and
 * keeps no reference to the view or its fds.
 */
typedef struct rsbfmq_desc {
    const int *fds;
    size_t nfds;
    const rsbfmq_grantor *grantors;
    size_t ngrantors;
    uint32_t quantum;
    uint32_t flags;
} rsbfmq_desc;

/*
 * What an attaching side demands beyond libfmq's own checks.
 *
 * max_capacity is the largest element count it will map; a peer-sized
 * mapping without a bound hands the peer this process's address space.
 * require_seal demands that every fd be a memfd sealed F_SEAL_SHRINK or an
 * ashmem region, so the peer cannot shrink it under the mapping (the next
 * access would raise SIGBUS). require_event_flag demands the EventFlag word,
 * without which nothing can wait.
 */
typedef struct rsbfmq_policy {
    size_t max_capacity;
    int require_seal;
    int require_event_flag;
} rsbfmq_policy;

/*
 * A counter: 8-aligned in the ring's memory (a page plus a multiple of 8),
 * which i386's ABI does not assume for uint64_t. Saying so keeps its atomics
 * inline instead of a libatomic call.
 */
typedef uint64_t rsbfmq_counter __attribute__((aligned(8)));

typedef struct rsbfmq_mapping {
    void *base;
    size_t len;
} rsbfmq_mapping;

/*
 * An attached or created queue. The fields are read-only to the caller;
 * `fd` is the memfd a create made (-1 after an attach), owned by the queue.
 */
typedef struct rsbfmq_queue {
    rsbfmq_counter *read;
    rsbfmq_counter *write;
    uint8_t *ring;
    uint32_t *flag;
    size_t ring_bytes;
    size_t quantum;
    size_t capacity;
    int fd;
    rsbfmq_grantor layout[4];
    size_t nlayout;
    rsbfmq_mapping maps[4];
} rsbfmq_queue;

/*
 * The elements a begin_write/begin_read covers: `first_len` before the
 * ring's end, `second_len` after its wrap (0 without one). Indices in
 * rsbfmq_regions_write/read run over both as one sequence.
 */
typedef struct rsbfmq_regions {
    uint8_t *first;
    size_t first_len;
    uint8_t *second;
    size_t second_len;
    size_t quantum;
} rsbfmq_regions;

/* ------------------------------------------------------------------ */
/* Shared memory                                                       */
/* ------------------------------------------------------------------ */

/*
 * The ashmem device number, found the way libcutils finds it: Android 11+
 * duplicates the node as /dev/ashmem<boot_id>, older releases have
 * /dev/ashmem. 0 when there is neither (a Linux host).
 */
static inline dev_t rsbfmq__ashmem_rdev(void) {
    char path[64] = "/dev/ashmem";
    struct stat st;
    FILE *f = fopen("/proc/sys/kernel/random/boot_id", "re");
    if (f) {
        char id[40] = {0};
        if (fgets(id, sizeof id, f)) {
            id[strcspn(id, "\r\n")] = '\0';
            snprintf(path, sizeof path, "/dev/ashmem%s", id);
        }
        fclose(f);
        if (stat(path, &st) == 0 && S_ISCHR(st.st_mode)) {
            return st.st_rdev;
        }
    }
    if (stat("/dev/ashmem", &st) == 0 && S_ISCHR(st.st_mode)) {
        return st.st_rdev;
    }
    return 0;
}

/*
 * libcutils __ashmem_is_ashmem: a character device with the ashmem rdev.
 * Android only, as in rsbinder-fmq: elsewhere no fd is ashmem, and none is
 * sent ASHMEM_GET_SIZE.
 */
static inline int rsbfmq_is_ashmem(int fd) {
#ifdef __ANDROID__
    struct stat st;
    dev_t rdev;
    if (fstat(fd, &st) != 0 || !S_ISCHR(st.st_mode)) {
        return 0;
    }
    rdev = rsbfmq__ashmem_rdev();
    return rdev != 0 && st.st_rdev == rdev;
#else
    (void)fd;
    (void)rsbfmq__ashmem_rdev;
    return 0;
#endif
}

/* Whether no holder of `fd` can shrink it: F_SEAL_SHRINK is set. */
static inline int rsbfmq_is_shrink_sealed(int fd) {
    int seals = fcntl(fd, F_GET_SEALS);
    return seals >= 0 && (seals & F_SEAL_SHRINK) != 0;
}

/* The region's size: st_size, or ASHMEM_GET_SIZE for ashmem (st_size is 0). */
static inline int rsbfmq_region_size(int fd, uint64_t *size) {
    struct stat st;
    int r;
    if (fstat(fd, &st) != 0) {
        return -errno;
    }
    if (st.st_size > 0) {
        *size = (uint64_t)st.st_size;
        return 0;
    }
    if (!rsbfmq_is_ashmem(fd)) {
        *size = 0;
        return 0;
    }
    /* ASHMEM_GET_SIZE = _IO(0x77, 4); the return value is the size. */
    r = ioctl(fd, 0x7704);
    if (r < 0) {
        return -errno;
    }
    *size = (uint64_t)r;
    return 0;
}

/* ------------------------------------------------------------------ */
/* Attach and create                                                   */
/* ------------------------------------------------------------------ */

static inline uint64_t rsbfmq__align8(uint64_t x) {
    return (x + 7) & ~(uint64_t)7;
}

/* Map `extent` bytes at `offset`, which need not be page-aligned. */
static inline int rsbfmq__map(int fd, uint64_t offset, uint64_t extent,
                              rsbfmq_mapping *m, void **at) {
    uint64_t page = (uint64_t)sysconf(_SC_PAGESIZE);
    uint64_t map_offset = offset - offset % page;
    uint64_t delta = offset - map_offset;
    void *base;
    if (extent == 0 || extent > SIZE_MAX - delta) {
        return -EINVAL;
    }
    base = mmap(NULL, (size_t)(extent + delta), PROT_READ | PROT_WRITE, MAP_SHARED,
                fd, (off_t)map_offset);
    if (base == MAP_FAILED) {
        return -errno;
    }
    m->base = base;
    m->len = (size_t)(extent + delta);
    *at = (uint8_t *)base + delta;
    return 0;
}

/*
 * Unmap everything and close a created queue's memfd. A queue that attach
 * or create left, failed or not, may be closed again; so may an all-zero
 * one (no layout, so its fd 0 is not taken for a memfd).
 */
static inline void rsbfmq_close(rsbfmq_queue *q) {
    size_t i;
    for (i = 0; i < 4; i++) {
        if (q->maps[i].base) {
            munmap(q->maps[i].base, q->maps[i].len);
        }
    }
    if (q->nlayout > 0 && q->fd >= 0) {
        close(q->fd);
    }
    memset(q, 0, sizeof *q);
    q->fd = -1;
}

/*
 * libfmq's initMemory/mapGrantorDescr checks, the policy's, and the i32::MAX
 * caps rsbinder-fmq applies (the AIDL offset is an int, and a ring must fit
 * a 32-bit size_t). The seal is checked before the size, since a seal never
 * comes off: the size read after it is a floor.
 */
static inline int rsbfmq__validate(const rsbfmq_desc *d, size_t quantum,
                                   const rsbfmq_policy *p, size_t *capacity) {
    static const uint64_t min_extent[4] = {8, 8, 1, 4};
    uint64_t sizes[4] = {0, 0, 0, 0};
    size_t n, used, i, j;
    if (d->flags != RSBFMQ_SYNCHRONIZED_READ_WRITE) {
        return -EINVAL;
    }
    if (quantum == 0 || d->quantum != quantum) {
        return -EINVAL;
    }
    n = d->ngrantors;
    if (n <= RSBFMQ_DATA) {
        return -EINVAL;
    }
    if (p->require_event_flag && n <= RSBFMQ_EVENT_FLAG_WORD) {
        return -EINVAL;
    }
    used = n < 4 ? n : 4;
    for (i = 0; i < used; i++) {
        const rsbfmq_grantor *g = &d->grantors[i];
        uint64_t size = 0;
        if (g->fd_index >= d->nfds) {
            return -EINVAL;
        }
        if (g->offset % 8 != 0) {
            return -EINVAL;
        }
        if (g->offset > (uint32_t)INT32_MAX || g->extent > (uint64_t)INT32_MAX) {
            return -EINVAL;
        }
        if (g->extent < min_extent[i]) {
            return -EINVAL;
        }
        /* Each fd is checked and sized once, by the first grantor that names it. */
        for (j = 0; j < i && d->grantors[j].fd_index != g->fd_index; j++) {
        }
        if (j < i) {
            size = sizes[j];
        } else {
            int fd = d->fds[g->fd_index];
            int r;
            if (p->require_seal && !(rsbfmq_is_shrink_sealed(fd) || rsbfmq_is_ashmem(fd))) {
                return -EINVAL;
            }
            r = rsbfmq_region_size(fd, &size);
            if (r != 0) {
                return r;
            }
        }
        sizes[i] = size;
        if ((uint64_t)g->offset + g->extent > size) {
            return -EINVAL;
        }
    }
    if (d->grantors[RSBFMQ_DATA].extent % quantum != 0) {
        return -EINVAL;
    }
    *capacity = (size_t)(d->grantors[RSBFMQ_DATA].extent / quantum);
    if (*capacity > p->max_capacity) {
        return -EINVAL;
    }
    for (i = 0; i < used; i++) {
        for (j = i + 1; j < used; j++) {
            const rsbfmq_grantor *a = &d->grantors[i];
            const rsbfmq_grantor *b = &d->grantors[j];
            if (a->fd_index == b->fd_index && a->offset < b->offset + b->extent &&
                b->offset < a->offset + a->extent) {
                return -EINVAL;
            }
        }
    }
    return 0;
}

static inline int rsbfmq__open(rsbfmq_queue *q, const rsbfmq_desc *d, size_t quantum,
                               const rsbfmq_policy *p) {
    const rsbfmq_grantor *g = d->grantors;
    size_t capacity;
    void *at;
    int r = rsbfmq__validate(d, quantum, p, &capacity);
    if (r != 0) {
        return r;
    }
    /* The counters and the word are mapped at their own size, not the peer's extent. */
    if ((r = rsbfmq__map(d->fds[g[0].fd_index], g[0].offset, 8, &q->maps[0], &at)) != 0) {
        goto fail;
    }
    q->read = (rsbfmq_counter *)at;
    if ((r = rsbfmq__map(d->fds[g[1].fd_index], g[1].offset, 8, &q->maps[1], &at)) != 0) {
        goto fail;
    }
    q->write = (rsbfmq_counter *)at;
    if ((r = rsbfmq__map(d->fds[g[2].fd_index], g[2].offset, g[2].extent, &q->maps[2], &at)) != 0) {
        goto fail;
    }
    q->ring = (uint8_t *)at;
    q->ring_bytes = (size_t)g[2].extent;
    if (d->ngrantors > RSBFMQ_EVENT_FLAG_WORD) {
        if ((r = rsbfmq__map(d->fds[g[3].fd_index], g[3].offset, 4, &q->maps[3], &at)) != 0) {
            goto fail;
        }
        q->flag = (uint32_t *)at;
    }
    q->quantum = quantum;
    q->capacity = capacity;
    return 0;
fail:
    rsbfmq_close(q);
    return r;
}

/*
 * Map a queue a peer described, after the checks above. The counters are
 * left as they are: libfmq's own constructor resets them by default
 * (resetPointers), so a libfmq peer must attach before the first write.
 * `quantum` is the element size this side reads and writes.
 */
static inline int rsbfmq_attach(rsbfmq_queue *q, const rsbfmq_desc *d, size_t quantum,
                                const rsbfmq_policy *p) {
    memset(q, 0, sizeof *q);
    q->fd = -1;
    return rsbfmq__open(q, d, quantum, p);
}

/*
 * Make a queue of `capacity` elements of `quantum` bytes in a new memfd,
 * with an EventFlag word when `event_flag` is set: libfmq's single-fd layout
 * (read counter at 0, write counter at 8, data at 16, the word at the next
 * multiple of 8). Every page is allocated now (fallocate), so the memory is
 * charged to this process rather than to whichever peer touches a page
 * first, and the fd is sealed GROW | SHRINK | SEAL. `16 + capacity *
 * quantum`, rounded up to 8, must not exceed INT32_MAX (libfmq's limit).
 *
 * memfd_create is called through syscall(): bionic's wrapper exists from
 * API 30, and the system call is on Android's app seccomp allowlist.
 */
static inline int rsbfmq_create(rsbfmq_queue *q, size_t capacity, size_t quantum,
                                int event_flag) {
    uint64_t data_bytes, offset = 0, end = 0, total, page;
    uint64_t sizes[4];
    rsbfmq_desc d;
    rsbfmq_policy p;
    size_t i, count = event_flag ? 4 : 3;
    int fd, r;
    memset(q, 0, sizeof *q);
    q->fd = -1;
    if (quantum == 0 || capacity == 0 || capacity > (uint64_t)INT32_MAX / quantum) {
        return -EINVAL;
    }
    data_bytes = (uint64_t)capacity * quantum;
    if (rsbfmq__align8(16 + data_bytes) > (uint64_t)INT32_MAX) {
        return -EINVAL;
    }
    sizes[0] = 8;
    sizes[1] = 8;
    sizes[2] = data_bytes;
    sizes[3] = 4;
    for (i = 0; i < count; i++) {
        uint64_t start = rsbfmq__align8(offset);
        q->layout[i].fd_index = 0;
        q->layout[i].offset = (uint32_t)start;
        q->layout[i].extent = sizes[i];
        end = start + sizes[i];
        offset = end;
    }
    q->nlayout = count;
    page = (uint64_t)sysconf(_SC_PAGESIZE);
    total = (end + page - 1) & ~(page - 1);

    fd = (int)syscall(__NR_memfd_create, "rsbinder-fmq", MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) {
        return -errno;
    }
    /* The 64-bit variants: `total` may reach 2^31, past a 32-bit off_t. */
    if (ftruncate64(fd, (off64_t)total) != 0 || fallocate64(fd, 0, 0, (off64_t)total) != 0 ||
        fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK | F_SEAL_SEAL) != 0) {
        r = -errno;
        close(fd);
        return r;
    }
    d.fds = &fd;
    d.nfds = 1;
    d.grantors = q->layout;
    d.ngrantors = count;
    d.quantum = (uint32_t)quantum;
    d.flags = RSBFMQ_SYNCHRONIZED_READ_WRITE;
    p.max_capacity = capacity;
    p.require_seal = 1;
    p.require_event_flag = event_flag;
    r = rsbfmq__open(q, &d, quantum, &p);
    if (r != 0) {
        close(fd);
        return r;
    }
    q->fd = fd;
    __atomic_store_n(q->read, 0, __ATOMIC_RELEASE);
    __atomic_store_n(q->write, 0, __ATOMIC_RELEASE);
    if (q->flag) {
        __atomic_store_n(q->flag, 0, __ATOMIC_SEQ_CST);
    }
    return 0;
}

/*
 * The descriptor a peer attaches a created queue with. It points into `q`;
 * the fd is the queue's own, so a transport that takes ownership of fds
 * needs a duplicate (fcntl F_DUPFD_CLOEXEC).
 */
static inline int rsbfmq_descriptor(const rsbfmq_queue *q, rsbfmq_desc *d) {
    if (q->fd < 0) {
        return -EINVAL;
    }
    d->fds = &q->fd;
    d->nfds = 1;
    d->grantors = q->layout;
    d->ngrantors = q->nlayout;
    d->quantum = (uint32_t)q->quantum;
    d->flags = RSBFMQ_SYNCHRONIZED_READ_WRITE;
    return 0;
}

/* ------------------------------------------------------------------ */
/* Counters and regions                                                */
/* ------------------------------------------------------------------ */

/*
 * Both counters, checked against a peer that writes them: read <= write, no
 * more in flight than the ring holds, no counter close enough to wrap, both
 * multiples of the element size. -EBADMSG otherwise.
 */
static inline int rsbfmq__positions(const rsbfmq_queue *q, uint64_t *read, uint64_t *write) {
    uint64_t w = __atomic_load_n(q->write, __ATOMIC_ACQUIRE);
    uint64_t r = __atomic_load_n(q->read, __ATOMIC_ACQUIRE);
    if (r > w || w - r > q->ring_bytes || w > UINT64_MAX - q->ring_bytes ||
        r % q->quantum != 0 || w % q->quantum != 0) {
        return -EBADMSG;
    }
    *read = r;
    *write = w;
    return 0;
}

/* Elements that can be read now. */
static inline int rsbfmq_available_to_read(const rsbfmq_queue *q, size_t *n) {
    uint64_t r, w;
    int e = rsbfmq__positions(q, &r, &w);
    if (e != 0) {
        return e;
    }
    *n = (size_t)((w - r) / q->quantum);
    return 0;
}

/* Elements that can be written now. */
static inline int rsbfmq_available_to_write(const rsbfmq_queue *q, size_t *n) {
    size_t used;
    int e = rsbfmq_available_to_read(q, &used);
    if (e != 0) {
        return e;
    }
    *n = q->capacity - used;
    return 0;
}

static inline void rsbfmq__regions_at(const rsbfmq_queue *q, uint64_t position, size_t n,
                                      rsbfmq_regions *out) {
    size_t offset = (size_t)(position % q->ring_bytes);
    size_t contiguous = (q->ring_bytes - offset) / q->quantum;
    out->first = q->ring + offset;
    out->second = q->ring;
    out->quantum = q->quantum;
    if (n > contiguous) {
        out->first_len = contiguous;
        out->second_len = n - contiguous;
    } else {
        out->first_len = n;
        out->second_len = 0;
    }
}

/* Reserve `n` elements to write; -EAGAIN when fewer are free. */
static inline int rsbfmq_begin_write(const rsbfmq_queue *q, size_t n, rsbfmq_regions *out) {
    uint64_t r, w;
    int e;
    if (n > q->capacity) {
        return -EAGAIN;
    }
    if ((e = rsbfmq__positions(q, &r, &w)) != 0) {
        return e;
    }
    if (q->capacity - (size_t)((w - r) / q->quantum) < n) {
        return -EAGAIN;
    }
    rsbfmq__regions_at(q, w, n, out);
    return 0;
}

/* Publish `n` elements written into the last begin_write's regions. */
static inline int rsbfmq_commit_write(rsbfmq_queue *q, size_t n) {
    uint64_t r, w;
    int e = rsbfmq__positions(q, &r, &w);
    if (e != 0) {
        return e;
    }
    if ((uint64_t)n > q->capacity - (w - r) / q->quantum) {
        return -EINVAL;
    }
    __atomic_store_n(q->write, w + (uint64_t)n * q->quantum, __ATOMIC_RELEASE);
    return 0;
}

/* The next `n` unread elements; -EAGAIN when fewer are available. */
static inline int rsbfmq_begin_read(const rsbfmq_queue *q, size_t n, rsbfmq_regions *out) {
    uint64_t r, w;
    int e;
    if (n > q->capacity) {
        return -EAGAIN;
    }
    if ((e = rsbfmq__positions(q, &r, &w)) != 0) {
        return e;
    }
    if ((size_t)((w - r) / q->quantum) < n) {
        return -EAGAIN;
    }
    rsbfmq__regions_at(q, r, n, out);
    return 0;
}

/* Free `n` elements read from the last begin_read's regions. */
static inline int rsbfmq_commit_read(rsbfmq_queue *q, size_t n) {
    uint64_t r, w;
    int e = rsbfmq__positions(q, &r, &w);
    if (e != 0) {
        return e;
    }
    if ((uint64_t)n > (w - r) / q->quantum) {
        return -EINVAL;
    }
    __atomic_store_n(q->read, r + (uint64_t)n * q->quantum, __ATOMIC_RELEASE);
    return 0;
}

/*
 * Copy `n` elements from `src` into the regions, starting at element
 * `start`. The copy sits between the counter's acquire load and release
 * store, as libfmq's memcpy does; nothing hands out a pointer the peer
 * could be rewriting while the caller interprets it.
 */
static inline int rsbfmq_regions_write(const rsbfmq_regions *r, size_t start, const void *src,
                                       size_t n) {
    size_t head;
    const uint8_t *s = (const uint8_t *)src;
    if (start > r->first_len + r->second_len || n > r->first_len + r->second_len - start) {
        return -EINVAL;
    }
    head = start < r->first_len ? r->first_len - start : 0;
    if (head > n) {
        head = n;
    }
    if (head) {
        memcpy(r->first + start * r->quantum, s, head * r->quantum);
    }
    if (n > head) {
        size_t at = start > r->first_len ? start - r->first_len : 0;
        memcpy(r->second + at * r->quantum, s + head * r->quantum, (n - head) * r->quantum);
    }
    return 0;
}

/* Copy `n` elements starting at element `start` of the regions into `dst`. */
static inline int rsbfmq_regions_read(const rsbfmq_regions *r, size_t start, void *dst,
                                      size_t n) {
    size_t head;
    uint8_t *d = (uint8_t *)dst;
    if (start > r->first_len + r->second_len || n > r->first_len + r->second_len - start) {
        return -EINVAL;
    }
    head = start < r->first_len ? r->first_len - start : 0;
    if (head > n) {
        head = n;
    }
    if (head) {
        memcpy(d, r->first + start * r->quantum, head * r->quantum);
    }
    if (n > head) {
        size_t at = start > r->first_len ? start - r->first_len : 0;
        memcpy(d + head * r->quantum, r->second + at * r->quantum, (n - head) * r->quantum);
    }
    return 0;
}

/* Write all `n` elements or none: -EAGAIN when they do not fit now. */
static inline int rsbfmq_write(rsbfmq_queue *q, const void *src, size_t n) {
    rsbfmq_regions r;
    int e = rsbfmq_begin_write(q, n, &r);
    if (e != 0) {
        return e;
    }
    rsbfmq_regions_write(&r, 0, src, n);
    return rsbfmq_commit_write(q, n);
}

/* Read all `n` elements or none: -EAGAIN when fewer are available. */
static inline int rsbfmq_read(rsbfmq_queue *q, void *dst, size_t n) {
    rsbfmq_regions r;
    int e = rsbfmq_begin_read(q, n, &r);
    if (e != 0) {
        return e;
    }
    rsbfmq_regions_read(&r, 0, dst, n);
    return rsbfmq_commit_read(q, n);
}

/* ------------------------------------------------------------------ */
/* EventFlag                                                           */
/* ------------------------------------------------------------------ */

/*
 * The futex system call taking this libc's struct timespec. A 32-bit build
 * whose time_t is 64 bits (glibc with _TIME_BITS=64) must use
 * futex_time64 (Linux 5.1+); everywhere else the two layouts agree.
 */
static inline long rsbfmq__futex(uint32_t *word, int op, uint32_t val,
                                 const struct timespec *deadline, uint32_t bits) {
#if defined(__NR_futex_time64) && !defined(__LP64__)
    if (sizeof(deadline->tv_sec) == 8) {
        return syscall(__NR_futex_time64, word, op, val, deadline, NULL, bits);
    }
#endif
    return syscall(__NR_futex, word, op, val, deadline, NULL, bits);
}

/*
 * rsbfmq_wake on an EventFlag word held apart from its queue — a mapping
 * of the word that outlives the queue, for a thread that wakes a waiter
 * after the queue may be gone (rsbinder_stream.h's death recipient).
 */
static inline int rsbfmq_word_wake(uint32_t *word, uint32_t bits) {
    uint32_t old;
    if (!word) {
        return -EINVAL;
    }
    if (bits == 0) {
        return 0;
    }
    old = __atomic_fetch_or(word, bits, __ATOMIC_SEQ_CST);
    /* A bit that was already set was already woken for. Not private: the word is shared. */
    if ((~old & bits) != 0 &&
        rsbfmq__futex(word, FUTEX_WAKE_BITSET, (uint32_t)INT_MAX, NULL, bits) < 0) {
        return -errno;
    }
    return 0;
}

/* Set `bits` and wake every waiter whose mask meets the ones that were clear. */
static inline int rsbfmq_wake(rsbfmq_queue *q, uint32_t bits) {
    return rsbfmq_word_wake(q->flag, bits);
}

/*
 * An absolute CLOCK_MONOTONIC deadline `ns` from now, for rsbfmq_wait_until.
 * Returns 1, and no deadline, when the moment does not fit this libc's
 * time_t (32 bits on 32-bit bionic): a wait that long waits without one,
 * as rsbinder-fmq does.
 */
static inline int rsbfmq_deadline_after(int64_t ns, struct timespec *out) {
    int64_t sec;
    long nsec;
    if (ns < 0 || clock_gettime(CLOCK_MONOTONIC, out) != 0) {
        return -EINVAL;
    }
    sec = (int64_t)out->tv_sec + ns / 1000000000;
    nsec = out->tv_nsec + (long)(ns % 1000000000);
    if (nsec >= 1000000000) {
        sec += 1;
        nsec -= 1000000000;
    }
    if (sizeof(time_t) < 8 && sec > INT32_MAX) {
        return 1;
    }
    out->tv_sec = (time_t)sec;
    out->tv_nsec = nsec;
    return 0;
}

/*
 * Wait until a bit of `bits` is set, clear those bits and store them in
 * `*set`. Returns at once when some already are. `deadline` is absolute
 * CLOCK_MONOTONIC, or NULL to wait without one.
 *
 * A wait consumes the bits in its mask, so a bit belongs to the side that
 * waits on it: a side that puts a bit it sets itself into its own mask
 * erases its own signal.
 */
static inline int rsbfmq_wait_until(rsbfmq_queue *q, uint32_t bits,
                                    const struct timespec *deadline, uint32_t *set) {
    if (!q->flag || bits == 0) {
        return -EINVAL;
    }
    for (;;) {
        uint32_t old = __atomic_fetch_and(q->flag, ~bits, __ATOMIC_SEQ_CST);
        if ((old & bits) != 0) {
            *set = old & bits;
            return 0;
        }
        /* Sleep only while the word reads as it did with our bits clear. */
        if (rsbfmq__futex(q->flag, FUTEX_WAIT_BITSET, old & ~bits, deadline, bits) < 0) {
            if (errno == ETIMEDOUT) {
                return -ETIMEDOUT;
            }
            if (errno != EAGAIN && errno != EINTR) {
                return -errno;
            }
        }
    }
}

/* rsbfmq_wait_until with a relative timeout in ns; negative waits without one. */
static inline int rsbfmq_wait(rsbfmq_queue *q, uint32_t bits, int64_t timeout_ns,
                              uint32_t *set) {
    struct timespec deadline;
    int r;
    if (timeout_ns < 0) {
        return rsbfmq_wait_until(q, bits, NULL, set);
    }
    r = rsbfmq_deadline_after(timeout_ns, &deadline);
    if (r < 0) {
        return r;
    }
    return rsbfmq_wait_until(q, bits, r == 0 ? &deadline : NULL, set);
}

/* The word as it is now, without clearing anything. */
static inline uint32_t rsbfmq_peek(const rsbfmq_queue *q) {
    return q->flag ? __atomic_load_n(q->flag, __ATOMIC_SEQ_CST) : 0;
}

#ifdef __cplusplus
} /* extern "C" */

#include <vector>

/*
 * The NDK backend's android.hardware.common.fmq.MQDescriptor, read into a
 * view. `fds` and `grantors` hold the arrays the view points at and must
 * outlive the attach. A negative fdIndex, offset, extent or quantum is
 * -EINVAL, as in rsbinder's TryFrom.
 */
template <class MQDescriptor>
inline int rsbfmq_desc_from_aidl(const MQDescriptor &mqd, std::vector<int> &fds,
                                 std::vector<rsbfmq_grantor> &grantors, rsbfmq_desc *out) {
    fds.clear();
    grantors.clear();
    for (const auto &fd : mqd.handle.fds) {
        fds.push_back(fd.get());
    }
    for (const auto &g : mqd.grantors) {
        if (g.fdIndex < 0 || g.offset < 0 || g.extent < 0) {
            return -EINVAL;
        }
        grantors.push_back(rsbfmq_grantor{static_cast<uint32_t>(g.fdIndex),
                                          static_cast<uint32_t>(g.offset),
                                          static_cast<uint64_t>(g.extent)});
    }
    if (mqd.quantum < 0) {
        return -EINVAL;
    }
    out->fds = fds.data();
    out->nfds = fds.size();
    out->grantors = grantors.data();
    out->ngrantors = grantors.size();
    out->quantum = static_cast<uint32_t>(mqd.quantum);
    out->flags = static_cast<uint32_t>(mqd.flags);
    return 0;
}

/* A created queue as the NDK backend's MQDescriptor; the fd is a duplicate it owns. */
template <class MQDescriptor>
inline int rsbfmq_desc_to_aidl(const rsbfmq_queue &q, MQDescriptor *mqd) {
    using Grantor = typename decltype(mqd->grantors)::value_type;
    int fd;
    size_t i;
    if (q.fd < 0) {
        return -EINVAL;
    }
    fd = fcntl(q.fd, F_DUPFD_CLOEXEC, 0);
    if (fd < 0) {
        return -errno;
    }
    mqd->handle.fds.clear();
    mqd->handle.fds.emplace_back(fd);
    mqd->handle.ints.clear();
    mqd->grantors.clear();
    for (i = 0; i < q.nlayout; i++) {
        Grantor g;
        g.fdIndex = static_cast<int32_t>(q.layout[i].fd_index);
        g.offset = static_cast<int32_t>(q.layout[i].offset);
        g.extent = static_cast<int64_t>(q.layout[i].extent);
        mqd->grantors.push_back(g);
    }
    mqd->quantum = static_cast<int32_t>(q.quantum);
    mqd->flags = static_cast<int32_t>(RSBFMQ_SYNCHRONIZED_READ_WRITE);
    return 0;
}
#endif /* __cplusplus */

#endif /* RSBINDER_FMQ_H */

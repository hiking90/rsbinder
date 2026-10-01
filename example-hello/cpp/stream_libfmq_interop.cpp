// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-7b P5 STAGE3, kernel half: a C++ client of `stream_probe serve`
// (rsbinder) whose ring is AOSP's own libfmq — `AidlMessageQueue` makes,
// maps and moves it, `EventFlag` waits and wakes on it. Only the records
// (a u32 LE header, then the payload) and the three event bits are written
// here, which is what contrib/ndk/rsbinder_stream.h says a platform peer
// with libfmq needs to add. Each subcommand is one case of
// run_stream_interop.sh and prints one `RESULT` line.
//
//   stream_libfmq_interop <svc> download <count> <ring>       libfmq consumes
//   stream_libfmq_interop <svc> failing <count> <code> <msg>  reads the end status
//   stream_libfmq_interop <svc> pause <count> <ring>          stops reading, then resumes
//   stream_libfmq_interop <svc> cancel <take> <ring>          cancels after <take>
//   stream_libfmq_interop <svc> orphan <ring>                 waits; the service is killed
//   stream_libfmq_interop <svc> upload <count> <ring>         libfmq produces
//   stream_libfmq_interop <svc> vanish <ring>                 produces; the service is killed

// libfmq's headers expect these first (see rsbinder-fmq/tests/cpp).
#include <algorithm>
#include <atomic>
#include <chrono>
#include <climits>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <fmq/AidlMessageQueue.h>
#include <fmq/EventFlag.h>

#include <aidl/rsbinder/stream/StreamEndpoint.h>
#include <aidl/streamdemo/IStreamDemo.h>
#include <android/binder_ibinder.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>

#include <errno.h>
#include <fcntl.h>
#include <unistd.h>

using aidl::android::hardware::common::fmq::SynchronizedReadWrite;
// IStreamDemo's streams carry `int`.
using StreamEndpoint = aidl::rsbinder::stream::StreamEndpoint<int32_t>;
using aidl::streamdemo::IStreamDemo;
using android::AidlMessageQueue;
using android::hardware::EventFlag;

namespace {

using Queue = AidlMessageQueue<int8_t, SynchronizedReadWrite>;

constexpr int64_t kSecond = 1000LL * 1000 * 1000;

// The record layer (plans/10-7b §2.3, §2.4); libfmq's FMQ_NOT_FULL / FMQ_NOT_EMPTY are bits 0 and 1.
constexpr uint32_t kNotFull = 0x01;
constexpr uint32_t kNotEmpty = 0x02;
constexpr uint32_t kCancel = 0x04;
constexpr size_t kHeader = 4;
constexpr size_t kEndReserve = 256;
constexpr size_t kEndFields = 8;
constexpr uint32_t kKindEnd = 0x80000000u;
constexpr uint32_t kLenMask = 0x7fffffffu;
constexpr int32_t kExIllegalState = -5;

void put_le32(uint8_t *p, uint32_t x) {
    for (int i = 0; i < 4; i++) p[i] = uint8_t(x >> (8 * i));
}
uint32_t get_le32(const uint8_t *p) {
    return uint32_t(p[0]) | uint32_t(p[1]) << 8 | uint32_t(p[2]) << 16 | uint32_t(p[3]) << 24;
}

// The queue and what a death callback touches. The callback holds a
// reference of its own, dropped in onUnlinked: libbinder may still be in
// the callback after AIBinder_unlinkToDeath returns.
struct Ring {
    std::unique_ptr<Queue> q;
    EventFlag *ef = nullptr;
    std::atomic<bool> dead{false};
    // What this side waits on, and so what a death wakes.
    uint32_t own_mask;

    Ring(std::unique_ptr<Queue> queue, uint32_t mask) : q(std::move(queue)), own_mask(mask) {
        if (!q->isValid() || q->getEventFlagWord() == nullptr ||
            EventFlag::createEventFlag(q->getEventFlagWord(), &ef) != android::OK) {
            fprintf(stderr, "libfmq: invalid queue or no event flag word\n");
            exit(2);
        }
    }
    ~Ring() { EventFlag::deleteEventFlag(&ef); }

    size_t capacity() const { return q->getQuantumCount(); }

    // One wait on `mask` until the deadline: 0, or -ETIMEDOUT.
    int wait(uint32_t mask, std::chrono::steady_clock::time_point deadline, uint32_t *state) {
        auto left = std::chrono::duration_cast<std::chrono::nanoseconds>(
                            deadline - std::chrono::steady_clock::now())
                            .count();
        if (left <= 0) return -ETIMEDOUT;
        android::status_t st = ef->wait(mask, state, left, /*retry=*/true);
        if (st == android::TIMED_OUT) return -ETIMEDOUT;
        return st == android::OK ? 0 : st;
    }
};

void on_binder_died(void *cookie) {
    auto &ring = *static_cast<std::shared_ptr<Ring> *>(cookie);
    ring->dead.store(true);
    ring->ef->wake(ring->own_mask);
}
void on_unlinked(void *cookie) { delete static_cast<std::shared_ptr<Ring> *>(cookie); }

struct Link {
    ndk::SpAIBinder binder;
    AIBinder_DeathRecipient *recipient;
    void *cookie;

    Link(ndk::SpAIBinder b, const std::shared_ptr<Ring> &ring)
        : binder(std::move(b)),
          recipient(AIBinder_DeathRecipient_new(on_binder_died)),
          cookie(new std::shared_ptr<Ring>(ring)) {
        AIBinder_DeathRecipient_setOnUnlinked(recipient, on_unlinked);
        binder_status_t st = AIBinder_linkToDeath(binder.get(), recipient, cookie);
        if (st != STATUS_OK) {
            fprintf(stderr, "linkToDeath: %d\n", st);
            exit(2);
        }
    }
    ~Link() {
        AIBinder_unlinkToDeath(binder.get(), recipient, cookie);
        AIBinder_DeathRecipient_delete(recipient);
    }
};

// The consumer's end: reads whole records, wakes NOT_FULL after each.
struct Consumer {
    std::shared_ptr<Ring> ring;
    bool ended = false;
    std::vector<uint8_t> buf;

    explicit Consumer(std::shared_ptr<Ring> r) : ring(std::move(r)) {}
    // As rsbinder's Drop for Receiver: a consumer that leaves early cancels.
    ~Consumer() {
        if (!ended) ring->ef->wake(kCancel);
    }

    int corrupted() {
        ended = true;
        ring->ef->wake(kCancel);
        return -EBADMSG;
    }

    // The next record into `buf`: 0, -EPIPE once the producer is gone and nothing is left.
    int recv(bool *is_end, int64_t timeout_ns) {
        if (ended) return -ENODATA;
        auto deadline = std::chrono::steady_clock::now() + std::chrono::nanoseconds(timeout_ns);
        for (;;) {
            // Before the look: a death seen after it may follow a record the look missed.
            bool dead = ring->dead.load();
            size_t available = ring->q->availableToRead();
            if (available == 0) {
                if (dead) {
                    ended = true;
                    return -EPIPE;
                }
                uint32_t state = 0;
                int r = ring->wait(kNotEmpty, deadline, &state);
                if (r != 0) return r;
                continue;
            }
            Queue::MemTransaction tx;
            uint8_t h[kHeader];
            if (available < kHeader || !ring->q->beginRead(kHeader, &tx) ||
                !tx.copyFrom(reinterpret_cast<int8_t *>(h), 0, kHeader)) {
                return corrupted();
            }
            uint32_t header = get_le32(h);
            bool end = (header & kKindEnd) != 0;
            size_t len = header & kLenMask;
            size_t most = end ? ring->capacity() - kHeader
                              : ring->capacity() - kEndReserve - kHeader;
            if (len == 0 || len > most || (end && len < kEndFields) ||
                available < kHeader + len) {
                return corrupted();
            }
            buf.resize(len);
            if (!ring->q->beginRead(kHeader + len, &tx) ||
                !tx.copyFrom(reinterpret_cast<int8_t *>(buf.data()), kHeader, len) ||
                !ring->q->commitRead(kHeader + len)) {
                return corrupted();
            }
            ring->ef->wake(kNotFull);
            if (end) ended = true;
            *is_end = end;
            return 0;
        }
    }

    // One item: 0 with `*item` or with `*end` set, else the recv code.
    int next(int32_t *item, bool *end, int64_t timeout = 10 * kSecond) {
        int r = recv(end, timeout);
        if (r != 0) return r;
        if (!*end) {
            if (buf.size() != 4) return -EPROTO;
            *item = int32_t(get_le32(buf.data()));  // an `int` in parcel encoding
        }
        return 0;
    }

    void end_fields(int32_t *exception, int32_t *code, std::string *message) const {
        *exception = int32_t(get_le32(buf.data()));
        *code = int32_t(get_le32(buf.data() + 4));
        message->assign(reinterpret_cast<const char *>(buf.data()) + kEndFields,
                        buf.size() - kEndFields);
    }
};

// The producer's end: items stay below the end reserve; the end record never waits.
struct Producer {
    std::shared_ptr<Ring> ring;
    bool canceled = false;
    bool ended = false;

    explicit Producer(std::shared_ptr<Ring> r) : ring(std::move(r)) {}
    // As rsbinder's Drop for Sink: a stream left without an end gets EX_ILLEGAL_STATE.
    ~Producer() {
        if (!ended && !ring->dead.load()) end(kExIllegalState, 0);
    }

    size_t max_item() const { return ring->capacity() - kEndReserve - kHeader; }

    int write(uint32_t header, const uint8_t *payload, size_t len, size_t limit) {
        const size_t n = kHeader + len;
        const bool is_end = (header & kKindEnd) != 0;
        auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(20);
        for (;;) {
            if (ring->dead.load()) return -EPIPE;
            // libfmq has no peek; the word is right there.
            uint32_t word = ring->q->getEventFlagWord()->load();
            if (!is_end && (canceled || (word & kCancel) != 0)) {
                canceled = true;
                return -ECANCELED;
            }
            size_t in_flight = ring->capacity() - ring->q->availableToWrite();
            if (in_flight + n <= limit) {
                Queue::MemTransaction tx;
                uint8_t h[kHeader];
                put_le32(h, header);
                if (!ring->q->beginWrite(n, &tx) ||
                    !tx.copyTo(reinterpret_cast<const int8_t *>(h), 0, kHeader) ||
                    !tx.copyTo(reinterpret_cast<const int8_t *>(payload), kHeader, len) ||
                    !ring->q->commitWrite(n)) {
                    return -EBADMSG;
                }
                ring->ef->wake(kNotEmpty);
                return 0;
            }
            // Items never pass the reserve, so an end without room means the counters moved.
            if (is_end) return -EBADMSG;
            // Not writeBlocking: it takes every wake as a retry and would never see CANCEL.
            uint32_t state = 0;
            int r = ring->wait(kNotFull | kCancel, deadline, &state);
            if (r != 0) return r;
            if ((state & kCancel) != 0) {
                canceled = true;
                return -ECANCELED;
            }
        }
    }

    int send(int32_t item) {
        uint8_t payload[4];
        put_le32(payload, uint32_t(item));
        return write(4, payload, 4, ring->capacity() - kEndReserve);
    }

    int end(int32_t exception, int32_t code) {
        uint8_t payload[kEndFields];
        put_le32(payload, uint32_t(exception));
        put_le32(payload + 4, uint32_t(code));
        int r = write(kKindEnd | kEndFields, payload, kEndFields, ring->capacity());
        if (r == 0) ended = true;
        return r;
    }
};

AIBinder_Class *token_class() {
    static AIBinder_Class *cls = AIBinder_Class_define(
        "rsbinder.stream.IStreamSink", [](void *) -> void * { return nullptr; },
        [](void *) {},
        [](AIBinder *, transaction_code_t, const AParcel *, AParcel *) -> binder_status_t {
            return STATUS_UNKNOWN_TRANSACTION;
        });
    return cls;
}

ndk::SpAIBinder new_token() { return ndk::SpAIBinder(AIBinder_new(token_class(), nullptr)); }

std::shared_ptr<IStreamDemo> connect(const char *name) {
    ndk::SpAIBinder binder(AServiceManager_checkService(name));
    if (binder.get() == nullptr) {
        fprintf(stderr, "no service %s\n", name);
        exit(2);
    }
    return IStreamDemo::fromBinder(binder);
}

void check(const ndk::ScopedAStatus &st, const char *what) {
    if (!st.isOk()) {
        fprintf(stderr, "%s: %s\n", what, st.getDescription().c_str());
        printf("RESULT ERROR %s\n", what);
        exit(1);
    }
}

// What backs the ring libfmq made: libcutils ashmem, or a memfd with its seals.
std::string memory_kind(const StreamEndpoint &endpoint) {
    int fd = endpoint.ring->handle.fds[0].get();
    char path[256];
    ssize_t n = readlink(("/proc/self/fd/" + std::to_string(fd)).c_str(), path, sizeof path - 1);
    if (n < 0) return "unknown";
    path[n] = 0;
    if (strstr(path, "/dev/ashmem") != nullptr) return "ashmem";
    if (strncmp(path, "/memfd:", 7) == 0) {
        int seals = fcntl(fd, F_GET_SEALS);
        return seals >= 0 && (seals & F_SEAL_SHRINK) ? "memfd:sealed" : "memfd:unsealed";
    }
    return "other";
}

// A download: libfmq makes the ring, the endpoint carries its descriptor.
struct Download {
    std::shared_ptr<Ring> ring;
    Consumer c;
    StreamEndpoint endpoint;

    explicit Download(size_t bytes)
        : ring(std::make_shared<Ring>(std::make_unique<Queue>(bytes, /*configureEventFlagWord=*/true),
                                      kNotEmpty)),
          c(ring) {
        endpoint.ring = ring->q->dupeDesc();
        endpoint.sink = new_token();
    }
};

// Read items until the end record; the count, whether each was next, and the end's fields.
void drain(Consumer &c, int *received, bool *ordered, int32_t *exception, int32_t *code,
           std::string *message, int *rc) {
    int32_t expected = 0, item;
    bool end = false;
    *ordered = true;
    for (;;) {
        *rc = c.next(&item, &end);
        if (*rc != 0) return;
        if (end) {
            c.end_fields(exception, code, message);
            return;
        }
        if (item != expected) *ordered = false;
        expected++;
        ++*received;
    }
}

int download(const char *svc, int count, size_t bytes) {
    auto demo = connect(svc);
    Download d(bytes);
    std::string mem = memory_kind(d.endpoint);
    check(demo->subscribe(d.endpoint, count, 0, 0, 0), "subscribe");
    int received = 0, rc;
    bool ordered;
    int32_t exception = 1, code = 0;
    std::string message;
    drain(d.c, &received, &ordered, &exception, &code, &message, &rc);
    printf("RESULT download %d %s rc=%d end=%d mem=%s\n", received,
           ordered ? "ordered" : "unordered", rc, exception, mem.c_str());
    return 0;
}

int failing(const char *svc, int count, int32_t code_in, const char *message_in) {
    auto demo = connect(svc);
    Download d(64 * 1024);
    check(demo->subscribeFailing(d.endpoint, count, code_in, message_in), "subscribeFailing");
    int received = 0, rc;
    bool ordered;
    int32_t exception = 1, code = 0;
    std::string message;
    drain(d.c, &received, &ordered, &exception, &code, &message, &rc);
    printf("RESULT failing %d rc=%d exception=%d code=%d message=%s\n", received, rc, exception,
           code, message.c_str());
    return 0;
}

// Poll `sent()` until it holds still for half a second.
int32_t settled_sent(const std::shared_ptr<IStreamDemo> &demo) {
    int32_t last = -1, now = 0;
    for (int still = 0; still < 5; std::this_thread::sleep_for(std::chrono::milliseconds(100))) {
        check(demo->sent(&now), "sent");
        still = now == last ? still + 1 : 0;
        last = now;
    }
    return now;
}

int pause_case(const char *svc, int count, size_t bytes) {
    auto demo = connect(svc);
    Download d(bytes);
    // `sent()` counts every stream this service has produced; this one's share is the difference.
    int32_t before = 0;
    check(demo->sent(&before), "sent");
    check(demo->subscribe(d.endpoint, count, 0, 0, 0), "subscribe");
    int32_t stalled = settled_sent(demo) - before;
    int received = 0, rc;
    bool ordered;
    int32_t exception = 1, code = 0;
    std::string message;
    drain(d.c, &received, &ordered, &exception, &code, &message, &rc);
    printf("RESULT pause stalled=%d %d %s rc=%d end=%d\n", stalled, received,
           ordered ? "ordered" : "unordered", rc, exception);
    return 0;
}

int cancel_case(const char *svc, int take, size_t bytes) {
    auto demo = connect(svc);
    Download d(bytes);
    check(demo->subscribe(d.endpoint, INT32_MAX, 0, 0, 0), "subscribe");
    int32_t item;
    bool end;
    int received = 0;
    while (received < take && d.c.next(&item, &end) == 0 && !end) received++;
    d.ring->ef->wake(kCancel);
    bool finished = false;
    int32_t error = 0;
    for (int i = 0; i < 100 && !finished; i++) {
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        check(demo->finished(&finished), "finished");
    }
    check(demo->lastError(&error), "lastError");
    printf("RESULT cancel %d finished=%d error=%d\n", received, finished ? 1 : 0, error);
    return 0;
}

int orphan(const char *svc, size_t bytes) {
    auto demo = connect(svc);
    Download d(bytes);
    Link watch(demo->asBinder(), d.ring);
    check(demo->subscribe(d.endpoint, INT32_MAX, 0, 0, 5000), "subscribe");
    printf("READY\n");
    fflush(stdout);
    int32_t item;
    bool end = false;
    int received = 0, rc;
    while ((rc = d.c.next(&item, &end, 20 * kSecond)) == 0 && !end) received++;
    printf("RESULT orphan %d rc=%d\n", received, rc);
    return 0;
}

// An upload: libfmq attaches to the ring rsbinder made, and watches the sink.
struct Upload {
    ndk::SpAIBinder token = new_token();
    StreamEndpoint endpoint;
    std::shared_ptr<Ring> ring;
    std::unique_ptr<Producer> p;
    std::unique_ptr<Link> watch;

    Upload(const std::shared_ptr<IStreamDemo> &demo, size_t bytes) {
        check(demo->upload(token, static_cast<int32_t>(bytes), &endpoint), "upload");
        if (!endpoint.ring) {
            fprintf(stderr, "upload: no ring in the endpoint\n");
            exit(2);
        }
        // Not libfmq's default: resetting would zero counters the consumer owns.
        ring = std::make_shared<Ring>(std::make_unique<Queue>(*endpoint.ring, /*resetPointers=*/false),
                                      kNotFull);
        p = std::make_unique<Producer>(ring);
        watch = std::make_unique<Link>(endpoint.sink, ring);
    }
};

int32_t uploaded_so_far(const std::shared_ptr<IStreamDemo> &demo) {
    int32_t n = 0;
    check(demo->uploaded(&n), "uploaded");
    return n;
}

int upload(const char *svc, int count, size_t bytes) {
    auto demo = connect(svc);
    int32_t before = uploaded_so_far(demo);
    Upload u(demo, bytes);
    int rc = 0;
    for (int32_t i = 0; i < count && rc == 0; i++) rc = u.p->send(i);
    if (rc == 0) rc = u.p->end(0, 0);
    bool finished = false, ordered = false;
    int32_t error = 0;
    for (int i = 0; i < 200 && !finished; i++) {
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        check(demo->uploadFinished(&finished), "uploadFinished");
    }
    int32_t uploaded = uploaded_so_far(demo) - before;
    check(demo->uploadOrdered(&ordered), "uploadOrdered");
    check(demo->uploadError(&error), "uploadError");
    printf("RESULT upload %d %s finished=%d error=%d rc=%d\n", uploaded,
           ordered ? "ordered" : "unordered", finished ? 1 : 0, error, rc);
    return 0;
}

int vanish(const char *svc, size_t bytes) {
    auto demo = connect(svc);
    Upload u(demo, bytes);
    printf("READY\n");
    fflush(stdout);
    int rc = 0, sent = 0;
    for (int32_t i = 0; rc == 0; i++) {
        rc = u.p->send(i);
        if (rc == 0) sent++;
        // Paced so the kill lands mid-stream, whether or not the ring is full then.
        if (i % 1000 == 999) std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
    printf("RESULT vanish %d rc=%d\n", sent, rc);
    return 0;
}

}  // namespace

// ---- what libfmq's headers and EventFlag.cpp want from libbase and
// libutils: as in fmq_interop.cpp, the device's libfmq.so is `std::__1`
// and cannot be linked from an NDK build, so EventFlag.cpp is compiled in
// and these stand in for its dependencies.

#include <android/log.h>
#include <time.h>

namespace android {
int64_t elapsedRealtimeNano() {
    struct timespec ts;
    clock_gettime(CLOCK_BOOTTIME, &ts);
    return int64_t(ts.tv_sec) * 1000000000LL + ts.tv_nsec;
}
namespace hardware {
namespace details {
void check(bool exp) {
    if (!exp) abort();
}
void check(bool exp, const char *message) {
    if (!exp) {
        __android_log_print(ANDROID_LOG_FATAL, "FMQ", "%s", message ? message : "check failed");
        abort();
    }
}
void logError(const std::string &m) { __android_log_print(ANDROID_LOG_ERROR, "FMQ", "%s", m.c_str()); }
void logWarning(const std::string &m) { __android_log_print(ANDROID_LOG_WARN, "FMQ", "%s", m.c_str()); }
void logDebug(const std::string &m) { __android_log_print(ANDROID_LOG_DEBUG, "FMQ", "%s", m.c_str()); }
void errorWriteLog(int tag, const char *info) {
    __android_log_print(ANDROID_LOG_ERROR, "FMQ", "%d: %s", tag, info ? info : "");
}
}  // namespace details
}  // namespace hardware
}  // namespace android

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <service> <case> ...\n", argv[0]);
        return 2;
    }
    // Death notifications arrive on a binder thread.
    ABinderProcess_setThreadPoolMaxThreadCount(2);
    ABinderProcess_startThreadPool();
    const char *svc = argv[1];
    std::string c = argv[2];
    auto n = [&](int i) { return static_cast<int>(strtol(argv[i], nullptr, 10)); };
    if (c == "download" && argc == 5) return download(svc, n(3), n(4));
    if (c == "failing" && argc == 6) return failing(svc, n(3), n(4), argv[5]);
    if (c == "pause" && argc == 5) return pause_case(svc, n(3), n(4));
    if (c == "cancel" && argc == 5) return cancel_case(svc, n(3), n(4));
    if (c == "orphan" && argc == 4) return orphan(svc, n(3));
    if (c == "upload" && argc == 5) return upload(svc, n(3), n(4));
    if (c == "vanish" && argc == 4) return vanish(svc, n(3));
    fprintf(stderr, "bad arguments for %s\n", c.c_str());
    return 2;
}

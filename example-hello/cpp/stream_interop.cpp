// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-7b P4 STAGE3, the NDK half: a C++ client of `stream_probe serve`
// (rsbinder) that speaks the kernel-binder stream through
// contrib/ndk/rsbinder_stream.h and rsbinder-fmq/c/rsbinder_fmq.h only —
// no libfmq, no rsbinder. Each subcommand is one case of
// run_stream_interop.sh and prints one `RESULT` line.
//
//   stream_interop <svc> download <count> <ring>       C consumes
//   stream_interop <svc> failing <count> <code> <msg>  C reads the end status
//   stream_interop <svc> pause <count> <ring>          C stops reading, then resumes
//   stream_interop <svc> cancel <take> <ring>          C cancels after <take>
//   stream_interop <svc> orphan <ring>                 C waits; the service is killed
//   stream_interop <svc> upload <count> <ring>         C produces
//   stream_interop <svc> vanish <ring>                 C produces; the service is killed
//   stream_interop <svc> toobig <ring>                 C sends an item past the ring

#include "rsbinder_stream.h"

#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <memory>
#include <string>
#include <thread>
#include <vector>

#include <aidl/rsbinder/stream/StreamEndpoint.h>
#include <aidl/streamdemo/IStreamDemo.h>
#include <android/binder_ibinder.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>

using aidl::rsbinder::stream::StreamEndpoint;
using aidl::streamdemo::IStreamDemo;

namespace {

constexpr int64_t kSecond = 1000LL * 1000 * 1000;

// A binder of this process with no methods: the sink of a download (the
// producer only links to its death) and the producer token of an upload
// (the consumer's Receiver picks the transport by it and watches it).
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

// A death link as rsbinder_stream.h asks for one: the stream's cookie, released by onUnlinked,
// which libbinder calls after any death callback and also when the link fails.
struct Link {
    ndk::SpAIBinder binder;
    AIBinder_DeathRecipient *recipient;
    void *cookie;

    Link(ndk::SpAIBinder b, void *c)
        : binder(std::move(b)), recipient(AIBinder_DeathRecipient_new(rsbs_on_binder_died)), cookie(c) {
        AIBinder_DeathRecipient_setOnUnlinked(recipient, rsbs_on_unlinked);
        binder_status_t st = AIBinder_linkToDeath(binder.get(), recipient, cookie);
        if (st != STATUS_OK) {
            fprintf(stderr, "linkToDeath: %d\n", st);
            exit(2);
        }
    }
    // Before or after the stream closes: the cookie keeps what the callback touches.
    ~Link() {
        AIBinder_unlinkToDeath(binder.get(), recipient, cookie);
        AIBinder_DeathRecipient_delete(recipient);
    }
};

// A download consumer: the ring and the endpoint that carries it.
struct Download {
    rsbs_consumer c;
    StreamEndpoint endpoint;
    ndk::SpAIBinder sink = new_token();
    std::vector<uint8_t> buf;

    explicit Download(size_t ring) {
        int r = rsbs_consumer_create(&c, ring);
        if (r == 0) r = rsbs_endpoint_make(c, sink, &endpoint);
        if (r != 0) {
            fprintf(stderr, "consumer: %d\n", r);
            exit(2);
        }
        buf.resize(rsbs_max_record(&c));
    }
    ~Download() { rsbs_consumer_close(&c); }

    // One record: 0 with the item or end fields, else the rsbs_recv code.
    int next(int32_t *item, bool *end, int64_t timeout = 10 * kSecond) {
        size_t len;
        int is_end;
        int r = rsbs_recv(&c, buf.data(), buf.size(), &len, &is_end, timeout);
        if (r != 0) return r;
        *end = is_end != 0;
        if (!*end) {
            if (len != 4) return -EPROTO;
            memcpy(item, buf.data(), 4);  // an `int` in parcel encoding: 4 bytes LE
        }
        last_len = len;
        return 0;
    }
    size_t last_len = 0;

    void end_fields(int32_t *exception, int32_t *code, std::string *message) {
        const char *m;
        size_t n;
        if (rsbs_end_fields(buf.data(), last_len, exception, code, &m, &n) == 0) {
            message->assign(m, n);
        }
    }
};

// Read items until the end record; the count, whether each was next, and the end's fields.
void drain(Download &d, int32_t first, int *received, bool *ordered, int32_t *exception,
           int32_t *code, std::string *message, int *rc) {
    int32_t expected = first, item;
    bool end = false;
    *ordered = true;
    for (;;) {
        *rc = d.next(&item, &end);
        if (*rc != 0) return;
        if (end) {
            d.end_fields(exception, code, message);
            return;
        }
        if (item != expected) *ordered = false;
        expected++;
        ++*received;
    }
}

int download(const char *svc, int count, size_t ring) {
    auto demo = connect(svc);
    Download d(ring);
    check(demo->subscribe(d.endpoint, count, 0, 0, 0), "subscribe");
    int received = 0, rc;
    bool ordered;
    int32_t exception = 1, code = 0;
    std::string message;
    drain(d, 0, &received, &ordered, &exception, &code, &message, &rc);
    printf("RESULT download %d %s rc=%d end=%d\n", received, ordered ? "ordered" : "unordered", rc,
           exception);
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
    drain(d, 0, &received, &ordered, &exception, &code, &message, &rc);
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

int pause_case(const char *svc, int count, size_t ring) {
    auto demo = connect(svc);
    Download d(ring);
    // `sent()` counts every stream this service has produced; this one's share is the difference.
    int32_t before = 0;
    check(demo->sent(&before), "sent");
    check(demo->subscribe(d.endpoint, count, 0, 0, 0), "subscribe");
    int32_t stalled = settled_sent(demo) - before;
    int received = 0, rc;
    bool ordered;
    int32_t exception = 1, code = 0;
    std::string message;
    drain(d, 0, &received, &ordered, &exception, &code, &message, &rc);
    printf("RESULT pause stalled=%d %d %s rc=%d end=%d\n", stalled, received,
           ordered ? "ordered" : "unordered", rc, exception);
    return 0;
}

int cancel_case(const char *svc, int take, size_t ring) {
    auto demo = connect(svc);
    Download d(ring);
    check(demo->subscribe(d.endpoint, INT32_MAX, 0, 0, 0), "subscribe");
    int32_t item;
    bool end;
    int received = 0;
    while (received < take && d.next(&item, &end) == 0 && !end) received++;
    rsbs_cancel(&d.c);
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

int orphan(const char *svc, size_t ring) {
    auto demo = connect(svc);
    Download d(ring);
    Link watch(demo->asBinder(), rsbs_consumer_death_cookie(&d.c));
    check(demo->subscribe(d.endpoint, INT32_MAX, 0, 0, 5000), "subscribe");
    printf("READY\n");
    fflush(stdout);
    int32_t item;
    bool end = false;
    int received = 0, rc;
    while ((rc = d.next(&item, &end, 20 * kSecond)) == 0 && !end) received++;
    printf("RESULT orphan %d rc=%d\n", received, rc);
    return 0;
}

// An upload producer on the endpoint the service returned, watching its sink.
struct Upload {
    rsbs_producer p;
    ndk::SpAIBinder token = new_token();
    StreamEndpoint endpoint;
    std::unique_ptr<Link> watch;

    Upload(const std::shared_ptr<IStreamDemo> &demo, size_t ring) {
        check(demo->upload(token, static_cast<int32_t>(ring), &endpoint), "upload");
        std::vector<int> fds;
        std::vector<rsbfmq_grantor> grantors;
        rsbfmq_desc desc;
        int r = rsbs_endpoint_ring(endpoint, fds, grantors, &desc);
        if (r == 0) r = rsbs_producer_attach(&p, &desc, 4 << 20);
        if (r != 0) {
            fprintf(stderr, "producer: %d\n", r);
            exit(2);
        }
        watch = std::make_unique<Link>(endpoint.sink, rsbs_producer_death_cookie(&p));
    }
    ~Upload() { rsbs_producer_close(&p); }

    int send(int32_t item) { return rsbs_send(&p, &item, 4, 1); }
};

// `uploaded()` counts every upload this service has taken; `before` is its value ahead of this one.
int32_t uploaded_so_far(const std::shared_ptr<IStreamDemo> &demo) {
    int32_t n = 0;
    check(demo->uploaded(&n), "uploaded");
    return n;
}

// Poll the service's consumer until it has left its loop.
void upload_report(const std::shared_ptr<IStreamDemo> &demo, int32_t before, const char *tag,
                   int extra_rc) {
    bool finished = false, ordered = false;
    int32_t uploaded = 0, error = 0;
    for (int i = 0; i < 200 && !finished; i++) {
        std::this_thread::sleep_for(std::chrono::milliseconds(50));
        check(demo->uploadFinished(&finished), "uploadFinished");
    }
    uploaded = uploaded_so_far(demo) - before;
    check(demo->uploadOrdered(&ordered), "uploadOrdered");
    check(demo->uploadError(&error), "uploadError");
    printf("RESULT %s %d %s finished=%d error=%d rc=%d\n", tag, uploaded,
           ordered ? "ordered" : "unordered", finished ? 1 : 0, error, extra_rc);
}

int upload(const char *svc, int count, size_t ring) {
    auto demo = connect(svc);
    int32_t before = uploaded_so_far(demo);
    Upload u(demo, ring);
    int rc = 0;
    for (int32_t i = 0; i < count && rc == 0; i++) rc = u.send(i);
    if (rc == 0) rc = rsbs_end(&u.p, RSBS_EX_NONE, 0, nullptr);
    upload_report(demo, before, "upload", rc);
    return 0;
}

int vanish(const char *svc, size_t ring) {
    auto demo = connect(svc);
    Upload u(demo, ring);
    printf("READY\n");
    fflush(stdout);
    int rc = 0, sent = 0;
    for (int32_t i = 0; rc == 0; i++) {
        rc = u.send(i);
        if (rc == 0) sent++;
        // Paced so the kill lands mid-stream, whether or not the ring is full then.
        if (i % 1000 == 999) std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
    printf("RESULT vanish %d rc=%d\n", sent, rc);
    return 0;
}

int toobig(const char *svc, size_t ring) {
    auto demo = connect(svc);
    int32_t before = uploaded_so_far(demo);
    Upload u(demo, ring);
    std::vector<uint8_t> big(rsbs_max_item(&u.p) + 1);
    int too_big = rsbs_send(&u.p, big.data(), big.size(), 1);
    int rc = 0;
    for (int32_t i = 0; i < 10 && rc == 0; i++) rc = u.send(i);
    if (rc == 0) rc = rsbs_end(&u.p, RSBS_EX_NONE, 0, nullptr);
    char tag[32];
    snprintf(tag, sizeof tag, "toobig %d", too_big);
    upload_report(demo, before, tag, rc);
    return 0;
}

}  // namespace

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
    if (c == "toobig" && argc == 4) return toobig(svc, n(3));
    fprintf(stderr, "bad arguments for %s\n", c.c_str());
    return 2;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 10-7b P5 STAGE3, RPC half: the stream contract over a session
// whose other end is the device's own libbinder `RpcSession`
// (libbinder_rpc_unstable.so). rsbinder serves `streamdemo.IStreamDemo`
// as the root of an `RpcServer` (`stream_probe serve-rpc`); this client
// implements the consumer and producer ends from the `.aidl` files
// rsbinder ships, compiled by the platform's `aidl --lang=ndk`.
//
// Over RPC there is no ring: the endpoint carries only the sink, and the
// items travel as `oneway IStreamSink.onBatch` calls paced by credit
// granted through `oneway IStreamSource.request`. A download sends
// rsbinder's batches over libbinder's incoming connections and this side's
// grants over its outgoing ones; an upload does the reverse. A death is
// the session going away.
//
//   stream_rpc_interop <sock> download <count> <maxBatchBytes> <credits>
//   stream_rpc_interop <sock> failing <count> <code> <message>
//   stream_rpc_interop <sock> cancel <takeBatches>
//   stream_rpc_interop <sock> orphan              the server is killed
//   stream_rpc_interop <sock> hang <credits>      takes the opening window, grants nothing
//   stream_rpc_interop <sock> upload <count> <credits> <batchItems>
//   stream_rpc_interop <sock> vanish              uploads until it is killed
//   stream_rpc_interop <sock> status              the service's counters, on a new session
//
// Each prints one `RESULT` line; run_stream_rpc_interop.sh judges them.

#include <aidl/rsbinder/stream/BnStreamSink.h>
#include <aidl/rsbinder/stream/BnStreamSource.h>
#include <aidl/rsbinder/stream/StreamEndpoint.h>
#include <aidl/streamdemo/IStreamDemo.h>
#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <climits>
#include <condition_variable>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <deque>
#include <memory>
#include <mutex>
#include <optional>
#include <string>
#include <thread>
#include <vector>

extern "C" {
// --- libbinder_rpc_unstable.so (AOSP binder_rpc_unstable.hpp) ---
struct ARpcSession;
ARpcSession *ARpcSession_new();
void ARpcSession_setMaxIncomingThreads(ARpcSession *session, size_t threads);
// `ARpcSession_setupUnixDomainClient` takes an init-created socket name under
// /dev/socket, so a path in /data/local/tmp goes through the preconnected form.
AIBinder *ARpcSession_setupPreconnectedClient(ARpcSession *session, int (*requestFd)(void *param),
                                              void *param, void (*paramDeleteFd)(void *param));
}

using aidl::rsbinder::stream::BnStreamSink;
using aidl::rsbinder::stream::BnStreamSource;
using aidl::rsbinder::stream::IStreamSink;
using aidl::rsbinder::stream::IStreamSource;
using aidl::rsbinder::stream::StreamEndpoint;
using aidl::streamdemo::IStreamDemo;
using ndk::ScopedAStatus;
using ndk::SpAIBinder;

using namespace std::chrono_literals;

namespace {

constexpr int32_t kExIllegalState = -5;

int connect_socket(void *param) {
    const char *path = static_cast<const char *>(param);
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) return -1;
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    if (connect(fd, reinterpret_cast<sockaddr *>(&addr), sizeof addr) < 0) {
        perror("connect");
        close(fd);
        return -1;
    }
    return fd;
}
void keep_param(void *) {}

// A session with incoming connections: the peer calls this side's sink or
// source from a thread of its own, and libbinder links an RPC death only
// with an incoming thread to deliver it. Never freed: the proxies need it.
std::shared_ptr<IStreamDemo> connect(const char *path) {
    ARpcSession *session = ARpcSession_new();
    ARpcSession_setMaxIncomingThreads(session, 2);
    SpAIBinder root(ARpcSession_setupPreconnectedClient(session, connect_socket,
                                                        const_cast<char *>(path), keep_param));
    if (root.get() == nullptr) {
        printf("RESULT ERROR session\n");
        exit(1);
    }
    std::shared_ptr<IStreamDemo> demo = IStreamDemo::fromBinder(root);
    if (!demo) {
        printf("RESULT ERROR not-an-IStreamDemo\n");
        exit(1);
    }
    return demo;
}

void check(const ScopedAStatus &st, const char *what) {
    if (!st.isOk()) {
        fprintf(stderr, "%s: %s\n", what, st.getDescription().c_str());
        printf("RESULT ERROR %s\n", what);
        exit(1);
    }
}

// --- consumer role ----------------------------------------------------

class Consumer : public BnStreamSink {
  public:
    ScopedAStatus onStart(const SpAIBinder &source, int32_t credits) override {
        std::shared_ptr<IStreamSource> peer = IStreamSource::fromBinder(source);
        if (!peer) {
            fprintf(stderr, "onStart: not an IStreamSource\n");
            return ScopedAStatus::ok();
        }
        // With back-pressure there is usually no call in flight to fail,
        // so this link is the only thing that reports the producer's end.
        // Leaked with the cookie, as this process lives for one case.
        static AIBinder_DeathRecipient *recipient = AIBinder_DeathRecipient_new(on_source_died);
        binder_status_t st = AIBinder_linkToDeath(source.get(), recipient, this);
        std::lock_guard<std::mutex> lock(mu_);
        if (st != STATUS_OK) fprintf(stderr, "linkToDeath(source): %d\n", st);
        linked_ = st == STATUS_OK;
        source_ = peer;
        credits_ = credits;
        cv_.notify_all();
        return ScopedAStatus::ok();
    }

    ScopedAStatus onBatch(const std::vector<uint8_t> &items, int32_t count) override {
        std::lock_guard<std::mutex> lock(mu_);
        // What the producer may have sent so far: its opening window plus the highest total granted.
        if (received_ >= credits_ + highest_total_) overrun_ = true;
        received_++;
        batches_.emplace_back(items, count);
        cv_.notify_all();
        return ScopedAStatus::ok();
    }

    ScopedAStatus onEnd(int32_t exception, int32_t serviceSpecific,
                        const std::optional<std::string> &message) override {
        std::lock_guard<std::mutex> lock(mu_);
        ended_ = true;
        end_exception_ = exception;
        end_service_specific_ = serviceSpecific;
        end_message_ = message;
        cv_.notify_all();
        return ScopedAStatus::ok();
    }

    // The next batch, or nothing once the stream is over, gone or stalled.
    std::optional<std::pair<std::vector<uint8_t>, int32_t>> next(
        std::chrono::seconds patience = 30s) {
        std::unique_lock<std::mutex> lock(mu_);
        if (!cv_.wait_for(lock, patience,
                          [&] { return !batches_.empty() || ended_ || source_died_; })) {
            stalled_ = true;
        }
        if (batches_.empty()) return std::nullopt;
        auto batch = std::move(batches_.front());
        batches_.pop_front();
        return batch;
    }

    void grant(int64_t total) {
        std::shared_ptr<IStreamSource> source;
        {
            std::lock_guard<std::mutex> lock(mu_);
            // Raised before the call: a batch this grant pays for can arrive before it returns.
            highest_total_ = std::max(highest_total_, total);
            source = source_;
        }
        if (!source) return;
        ScopedAStatus st = source->request(total);
        if (!st.isOk()) {
            fprintf(stderr, "request(%lld): %s\n", (long long)total, st.getDescription().c_str());
        }
    }

    // Totals 1, 1, 0: a producer that sums them sends two batches, not one.
    // Returns the batches received once it has gone quiet.
    int64_t hold() {
        grant(1);
        grant(1);
        grant(0);
        std::unique_lock<std::mutex> lock(mu_);
        int64_t want = credits_ + 1;
        cv_.wait_for(lock, 10s, [&] { return received_ >= want; });
        // Long enough for a batch sent on wrongly counted credit to land.
        cv_.wait_for(lock, 1s, [&] { return received_ > want; });
        return received_;
    }

    // Until the opening window has arrived, then quiet for half a second.
    int64_t wait_window() {
        std::unique_lock<std::mutex> lock(mu_);
        cv_.wait_for(lock, 10s, [&] { return source_ && received_ >= credits_; });
        cv_.wait_for(lock, 500ms, [&] { return received_ > credits_; });
        return received_;
    }

    std::shared_ptr<IStreamSource> source() {
        std::lock_guard<std::mutex> lock(mu_);
        return source_;
    }

    std::mutex mu_;
    std::condition_variable cv_;
    std::shared_ptr<IStreamSource> source_;
    std::deque<std::pair<std::vector<uint8_t>, int32_t>> batches_;
    int64_t credits_ = 0;
    int64_t received_ = 0;
    int64_t highest_total_ = 0;
    bool linked_ = false;
    bool overrun_ = false;
    bool ended_ = false;
    bool stalled_ = false;
    bool source_died_ = false;
    int32_t end_exception_ = 0;
    int32_t end_service_specific_ = 0;
    std::optional<std::string> end_message_;

  private:
    static void on_source_died(void *cookie) {
        auto *self = static_cast<Consumer *>(cookie);
        std::lock_guard<std::mutex> lock(self->mu_);
        self->source_died_ = true;
        self->cv_.notify_all();
    }
};

// A batch is the items' parcel bytes, so a parcel is what reads them.
bool decode(const std::vector<uint8_t> &bytes, int32_t count, int32_t *expected, bool *ordered) {
    AParcel *parcel = AParcel_create();
    bool ok = AParcel_unmarshal(parcel, bytes.data(), bytes.size()) == STATUS_OK &&
              AParcel_setDataPosition(parcel, 0) == STATUS_OK;
    for (int32_t i = 0; ok && i < count; i++) {
        int32_t item = 0;
        ok = AParcel_readInt32(parcel, &item) == STATUS_OK;
        if (ok && item != *expected) *ordered = false;
        if (ok) (*expected)++;
    }
    ok = ok && AParcel_getDataPosition(parcel) == static_cast<int32_t>(bytes.size());
    AParcel_delete(parcel);
    return ok;
}

struct Drained {
    int32_t items = 0;
    bool ordered = true;
    bool decoded = true;
    int64_t batches = 0;
    int64_t held = -1;
};

// Take batches, granting each one back as it is drained, until the end,
// a death, a stall, or `stop_after` batches.
Drained drain(const std::shared_ptr<Consumer> &consumer, bool probe_duplicates,
              int64_t stop_after = INT64_MAX) {
    Drained got;
    while (got.batches < stop_after) {
        auto batch = consumer->next();
        if (!batch) break;
        if (!decode(batch->first, batch->second, &got.items, &got.ordered)) {
            got.decoded = false;
            break;
        }
        got.batches++;
        if (probe_duplicates && got.batches == 1) {
            got.held = consumer->hold();
            continue;
        }
        consumer->grant(got.batches);
    }
    return got;
}

std::shared_ptr<Consumer> subscribed(const std::shared_ptr<IStreamDemo> &demo, int32_t count,
                                     int32_t max_batch_bytes, int32_t credits, int32_t delay) {
    auto consumer = ndk::SharedRefBase::make<Consumer>();
    StreamEndpoint endpoint;
    endpoint.sink = consumer->asBinder();
    check(demo->subscribe(endpoint, count, max_batch_bytes, credits, delay), "subscribe");
    return consumer;
}

int download(const char *sock, int32_t count, int32_t max_batch_bytes, int32_t credits) {
    auto demo = connect(sock);
    auto consumer = subscribed(demo, count, max_batch_bytes, credits, 0);
    Drained got = drain(consumer, true);
    std::lock_guard<std::mutex> lock(consumer->mu_);
    printf("RESULT download %d %s end=%d held=%lld overrun=%d decoded=%d stalled=%d\n", got.items,
           got.ordered ? "ordered" : "unordered", consumer->ended_ ? consumer->end_exception_ : 1,
           (long long)got.held, consumer->overrun_, got.decoded, consumer->stalled_);
    return 0;
}

int failing(const char *sock, int32_t count, int32_t code, const char *message) {
    auto demo = connect(sock);
    auto consumer = ndk::SharedRefBase::make<Consumer>();
    StreamEndpoint endpoint;
    endpoint.sink = consumer->asBinder();
    check(demo->subscribeFailing(endpoint, count, code, message), "subscribeFailing");
    Drained got = drain(consumer, false);
    std::lock_guard<std::mutex> lock(consumer->mu_);
    printf("RESULT failing %d ended=%d exception=%d code=%d message=%s\n", got.items,
           consumer->ended_, consumer->end_exception_, consumer->end_service_specific_,
           consumer->end_message_ ? consumer->end_message_->c_str() : "<none>");
    return 0;
}

int cancel_case(const char *sock, int64_t take) {
    auto demo = connect(sock);
    // 64-byte batches, so the producer is always a few batches ahead of the cancel.
    auto consumer = subscribed(demo, INT32_MAX, 64, 4, 0);
    Drained got = drain(consumer, false, take);
    std::shared_ptr<IStreamSource> source = consumer->source();
    if (source) check(source->cancel(), "cancel");
    bool finished = false;
    int32_t error = 0;
    for (int i = 0; i < 100 && !finished; i++) {
        std::this_thread::sleep_for(50ms);
        check(demo->finished(&finished), "finished");
    }
    check(demo->lastError(&error), "lastError");
    printf("RESULT cancel %lld finished=%d error=%d\n", (long long)got.batches, finished ? 1 : 0,
           error);
    return 0;
}

int orphan(const char *sock) {
    auto demo = connect(sock);
    // Paced, so the server is killed with the stream running.
    auto consumer = subscribed(demo, INT32_MAX, 64, 4, 5000);
    printf("READY\n");
    fflush(stdout);
    Drained got = drain(consumer, false);
    std::lock_guard<std::mutex> lock(consumer->mu_);
    printf("RESULT orphan %d linked=%d died=%d ended=%d stalled=%d\n", got.items,
           consumer->linked_, consumer->source_died_, consumer->ended_, consumer->stalled_);
    return 0;
}

// Takes the opening window, grants nothing, and waits to be killed: the
// producer is then parked for credit with nothing in flight.
int hang(const char *sock, int32_t credits) {
    auto demo = connect(sock);
    int32_t before = 0;
    check(demo->sent(&before), "sent");
    auto consumer = subscribed(demo, INT32_MAX, 64, credits, 0);
    int64_t batches = consumer->wait_window();
    printf("READY batches=%lld\n", (long long)batches);
    fflush(stdout);
    std::this_thread::sleep_for(60s);
    printf("RESULT hang ERROR not-killed\n");
    return 1;
}

// --- producer role ----------------------------------------------------

class Source : public BnStreamSource {
  public:
    explicit Source(int64_t credits) : credit_(credits) {}

    ScopedAStatus request(int64_t total) override {
        std::lock_guard<std::mutex> lock(mu_);
        // Keep the highest, not a sum.
        if (total > highest_total_) {
            credit_ += total - highest_total_;
            highest_total_ = total;
            cv_.notify_all();
        }
        return ScopedAStatus::ok();
    }

    ScopedAStatus cancel() override {
        std::lock_guard<std::mutex> lock(mu_);
        canceled_ = true;
        cv_.notify_all();
        return ScopedAStatus::ok();
    }

    void sink_died() {
        std::lock_guard<std::mutex> lock(mu_);
        dead_ = true;
        cv_.notify_all();
    }

    // Spend one credit, waiting for it: false once cancelled, gone, or stalled.
    bool take_credit() {
        std::unique_lock<std::mutex> lock(mu_);
        if (!cv_.wait_for(lock, 30s, [&] { return credit_ > 0 || canceled_ || dead_; })) {
            stalled_ = true;
        }
        if (canceled_ || dead_ || stalled_) return false;
        credit_--;
        return true;
    }

    std::mutex mu_;
    std::condition_variable cv_;
    int64_t credit_;
    int64_t highest_total_ = 0;
    bool canceled_ = false;
    bool dead_ = false;
    bool stalled_ = false;
};

void on_sink_died(void *cookie) { (*static_cast<std::shared_ptr<Source> *>(cookie))->sink_died(); }

std::vector<uint8_t> encode(int32_t first, int32_t n) {
    AParcel *parcel = AParcel_create();
    for (int32_t i = 0; i < n; i++) AParcel_writeInt32(parcel, first + i);
    std::vector<uint8_t> bytes(AParcel_getDataSize(parcel));
    AParcel_marshal(parcel, bytes.data(), 0, bytes.size());
    AParcel_delete(parcel);
    return bytes;
}

// An upload: the service's endpoint holds only its sink; this side opens the stream on it.
struct Upload {
    SpAIBinder token;
    std::shared_ptr<IStreamSink> sink;
    std::shared_ptr<Source> source;

    Upload(const std::shared_ptr<IStreamDemo> &demo, int32_t credits) {
        source = ndk::SharedRefBase::make<Source>(credits);
        // The service's Receiver watches this binder; the source is a binder of this process too.
        token = source->asBinder();
        StreamEndpoint endpoint;
        check(demo->upload(token, 0, &endpoint), "upload");
        if (endpoint.ring) {
            printf("RESULT ERROR ring-over-rpc\n");
            exit(1);
        }
        sink = IStreamSink::fromBinder(endpoint.sink);
        if (!sink) {
            printf("RESULT ERROR not-an-IStreamSink\n");
            exit(1);
        }
        // A producer parked for credit has no call in flight either. Leaked with the process.
        static AIBinder_DeathRecipient *recipient = AIBinder_DeathRecipient_new(on_sink_died);
        binder_status_t st = AIBinder_linkToDeath(endpoint.sink.get(), recipient,
                                                  new std::shared_ptr<Source>(source));
        if (st != STATUS_OK) fprintf(stderr, "linkToDeath(sink): %d\n", st);
        check(sink->onStart(source->asBinder(), credits), "onStart");
    }

    // `count` items in batches of `per_batch`, each on a credit: the items sent.
    int32_t produce(int32_t count, int32_t per_batch, std::chrono::milliseconds pace = 0ms) {
        int32_t next = 0;
        while (next < count && source->take_credit()) {
            int32_t n = std::min(per_batch, count - next);
            std::vector<uint8_t> bytes = encode(next, n);
            ScopedAStatus st = sink->onBatch(bytes, n);
            if (!st.isOk()) {
                fprintf(stderr, "onBatch: %s\n", st.getDescription().c_str());
                break;
            }
            next += n;
            if (pace.count() > 0) std::this_thread::sleep_for(pace);
        }
        return next;
    }
};

int32_t uploaded_so_far(const std::shared_ptr<IStreamDemo> &demo) {
    int32_t n = 0;
    check(demo->uploaded(&n), "uploaded");
    return n;
}

int upload(const char *sock, int32_t count, int32_t credits, int32_t per_batch) {
    auto demo = connect(sock);
    int32_t before = uploaded_so_far(demo);
    Upload u(demo, credits);
    int32_t sent = u.produce(count, per_batch);
    ScopedAStatus st = sent == count ? u.sink->onEnd(0, 0, std::nullopt)
                                     : u.sink->onEnd(kExIllegalState, 0, "stopped early");
    if (!st.isOk()) fprintf(stderr, "onEnd: %s\n", st.getDescription().c_str());
    bool finished = false, ordered = false;
    int32_t error = 0;
    for (int i = 0; i < 200 && !finished; i++) {
        std::this_thread::sleep_for(50ms);
        check(demo->uploadFinished(&finished), "uploadFinished");
    }
    int32_t uploaded = uploaded_so_far(demo) - before;
    check(demo->uploadOrdered(&ordered), "uploadOrdered");
    check(demo->uploadError(&error), "uploadError");
    std::lock_guard<std::mutex> lock(u.source->mu_);
    // Not `canceled_`: rsbinder's Receiver cancels on drop even after a clean end.
    printf("RESULT upload %d %s finished=%d error=%d sent=%d\n", uploaded,
           ordered ? "ordered" : "unordered", finished ? 1 : 0, error, sent);
    return 0;
}

// Uploads, paced, until the script kills this process.
int vanish(const char *sock) {
    auto demo = connect(sock);
    Upload u(demo, 4);
    printf("READY\n");
    fflush(stdout);
    int32_t sent = u.produce(INT32_MAX, 16, 20ms);
    printf("RESULT vanish ERROR not-killed sent=%d\n", sent);
    return 1;
}

int status(const char *sock) {
    auto demo = connect(sock);
    int32_t sent = 0, error = 0, uploaded = 0, up_error = 0;
    bool finished = false, up_finished = false, up_ordered = false;
    check(demo->sent(&sent), "sent");
    check(demo->finished(&finished), "finished");
    check(demo->lastError(&error), "lastError");
    check(demo->uploaded(&uploaded), "uploaded");
    check(demo->uploadOrdered(&up_ordered), "uploadOrdered");
    check(demo->uploadFinished(&up_finished), "uploadFinished");
    check(demo->uploadError(&up_error), "uploadError");
    printf("RESULT status sent=%d finished=%d error=%d up_received=%d up_ordered=%d "
           "up_finished=%d up_error=%d\n",
           sent, finished, error, uploaded, up_ordered, up_finished, up_error);
    return 0;
}

}  // namespace

int main(int argc, char **argv) {
    if (argc < 3) {
        fprintf(stderr, "usage: %s <socket> <case> ...\n", argv[0]);
        return 2;
    }
    const char *sock = argv[1];
    std::string c = argv[2];
    auto n = [&](int i) { return static_cast<int32_t>(strtol(argv[i], nullptr, 10)); };
    if (c == "download" && argc == 6) return download(sock, n(3), n(4), n(5));
    if (c == "failing" && argc == 6) return failing(sock, n(3), n(4), argv[5]);
    if (c == "cancel" && argc == 4) return cancel_case(sock, n(3));
    if (c == "orphan" && argc == 3) return orphan(sock);
    if (c == "hang" && argc == 4) return hang(sock, n(3));
    if (c == "upload" && argc == 6) return upload(sock, n(3), n(4), n(5));
    if (c == "vanish" && argc == 3) return vanish(sock);
    if (c == "status" && argc == 3) return status(sock);
    fprintf(stderr, "bad arguments for %s\n", c.c_str());
    return 2;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 12-fmq F2 STAGE3 — `fmqinterop.IFmqPeer` on AOSP's own stack:
// libbinder_ndk carries the `MQDescriptor`, libfmq (`AidlMessageQueue`,
// the device's libfmq.so) runs the ring, libcutils makes the shared
// memory (ashmem on this device, memfd with `sys.use_memfd=true`). The
// other end is rsbinder (`tests/src/fmq_peer.rs` through `fmq_probe`),
// and either side can be the service.
//
//   fmq_interop serve  <name>
//   fmq_interop client <name> <server-queue|client-queue|corrupt|all> [count] [capacity]
//
// `client` prints the same `RESULT` lines as `fmq_probe client`, and
// exits non-zero when one does not hold. `run_fmq_interop.sh` builds and
// drives both.

// libfmq's headers expect these first (see rsbinder-fmq/tests/cpp).
#include <algorithm>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <climits>
#include <cstring>
#include <memory>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

#include <cutils/ashmem.h>
#include <cutils/native_handle.h>
#include <fmq/MQDescriptorBase.h>
#include <fmq/AidlMessageQueue.h>

#include <aidl/fmqinterop/BnFmqPeer.h>
#include <aidl/fmqinterop/IFmqPeer.h>
#include <android/binder_manager.h>
#include <android/binder_process.h>

#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

using aidl::android::hardware::common::fmq::MQDescriptor;
using aidl::android::hardware::common::fmq::SynchronizedReadWrite;
using aidl::fmqinterop::BnFmqPeer;
using aidl::fmqinterop::IFmqPeer;
using aidl::fmqinterop::QueueView;
using android::AidlMessageQueue;
using ndk::ScopedAStatus;

using Queue = AidlMessageQueue<int32_t, SynchronizedReadWrite>;
using Desc = MQDescriptor<int32_t, SynchronizedReadWrite>;

static const int64_t kWaitNs = 10LL * 1000 * 1000 * 1000;
static const int32_t kDefaultCount = 5000;
static const int32_t kDefaultCapacity = 64;

static int64_t expectedSum(int32_t count) {
    int64_t sum = 0;
    for (int64_t i = 0; i < count; i++) sum += i;
    return sum;
}

// Write 0..count in chunks of five, waiting for room.
static bool writeAll(Queue& q, int32_t count, std::string* why) {
    int32_t next = 0;
    int32_t chunk[5];
    while (next < count) {
        size_t n = std::min<size_t>(5, size_t(count - next));
        for (size_t i = 0; i < n; i++) chunk[i] = next + int32_t(i);
        if (!q.writeBlocking(chunk, n, kWaitNs)) {
            *why = "writeBlocking failed at " + std::to_string(next);
            return false;
        }
        next += int32_t(n);
    }
    return true;
}

// Read count items in chunks of seven, waiting for them.
static bool readAll(Queue& q, int32_t count, int64_t* sum, bool* ordered, std::string* why) {
    int32_t next = 0;
    *sum = 0;
    *ordered = true;
    int32_t chunk[7];
    while (next < count) {
        size_t n = std::min<size_t>(7, size_t(count - next));
        if (!q.readBlocking(chunk, n, kWaitNs)) {
            *why = "readBlocking failed at " + std::to_string(next);
            return false;
        }
        for (size_t i = 0; i < n; i++) {
            if (chunk[i] != next) *ordered = false;
            *sum += chunk[i];
            next++;
        }
    }
    return true;
}

// What the fd behind a descriptor is, as this process sees it.
static std::string memoryKind(const Desc& desc) {
    if (desc.handle.fds.empty()) return "none";
    int fd = desc.handle.fds[0].get();
    char path[256];
    ssize_t n = readlink(("/proc/self/fd/" + std::to_string(fd)).c_str(), path, sizeof(path) - 1);
    if (n < 0) return "unknown";
    path[n] = 0;
    if (strstr(path, "/dev/ashmem") != nullptr) return "ashmem";
    if (strncmp(path, "/memfd:", 7) == 0) {
        int seals = fcntl(fd, F_GET_SEALS);
        return (seals >= 0 && (seals & F_SEAL_SHRINK)) ? "memfd:sealed" : "memfd:unsealed";
    }
    return "other";
}

// ---- the service ----------------------------------------------------------

class Peer : public BnFmqPeer {
  public:
    ScopedAStatus create(int32_t capacity, Desc* out) override {
        std::lock_guard<std::mutex> lock(mu_);
        auto q = std::make_unique<Queue>(size_t(capacity), /*configureEventFlagWord=*/true);
        if (!q->isValid()) return fail("create: invalid queue");
        *out = q->dupeDesc();
        queue_ = std::move(q);
        return ScopedAStatus::ok();
    }

    ScopedAStatus adopt(const Desc& desc, int32_t* out) override {
        std::lock_guard<std::mutex> lock(mu_);
        std::string kind = memoryKind(desc);
        auto q = std::make_unique<Queue>(desc /* resetPointers = true, libfmq's default */);
        if (!q->isValid()) return fail("adopt: invalid queue");
        *out = int32_t(q->getQuantumCount());
        fprintf(stderr, "fmq_interop: adopted a %d-item queue (%s)\n", *out, kind.c_str());
        queue_ = std::move(q);
        return ScopedAStatus::ok();
    }

    ScopedAStatus produce(int32_t count, int32_t* out) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (!queue_) return noQueue();
        std::string why;
        if (!writeAll(*queue_, count, &why)) return fail(why);
        *out = count;
        return ScopedAStatus::ok();
    }

    ScopedAStatus consume(int32_t count, int64_t* out) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (!queue_) return noQueue();
        std::string why;
        bool ordered;
        if (!readAll(*queue_, count, out, &ordered, &why)) return fail(why);
        return ScopedAStatus::ok();
    }

    // libfmq reports counts as size_t and has no error path here: a ring
    // whose counters break its invariants yields whatever the arithmetic
    // says, which is what this records.
    ScopedAStatus view(int32_t count, QueueView* out) override {
        std::lock_guard<std::mutex> lock(mu_);
        if (!queue_) return noQueue();
        out->availableToRead = int64_t(queue_->availableToRead());
        out->availableToWrite = int64_t(queue_->availableToWrite());
        std::vector<int32_t> items(size_t(std::max(count, 0)));
        out->sum = 0;
        if (queue_->read(items.data(), items.size())) {
            out->readResult = 1;
            for (int32_t i : items) out->sum += i;
        } else {
            out->readResult = 0;
        }
        return ScopedAStatus::ok();
    }

  private:
    static ScopedAStatus fail(const std::string& why) {
        return ScopedAStatus::fromServiceSpecificErrorWithMessage(-1, why.c_str());
    }
    static ScopedAStatus noQueue() {
        return ScopedAStatus::fromExceptionCodeWithMessage(EX_ILLEGAL_STATE, "no queue yet");
    }

    std::mutex mu_;
    std::unique_ptr<Queue> queue_;
};

static int runServe(const char* name) {
    ABinderProcess_setThreadPoolMaxThreadCount(4);
    auto peer = ndk::SharedRefBase::make<Peer>();
    binder_status_t st = AServiceManager_addService(peer->asBinder().get(), name);
    if (st != STATUS_OK) {
        fprintf(stderr, "addService(%s) failed: %d\n", name, st);
        return 1;
    }
    printf("SERVING %s\n", name);
    fflush(stdout);
    ABinderProcess_joinThreadPool();
    return 0;
}

// ---- the driver -----------------------------------------------------------

struct Outcome {
    std::string line;
    bool ok;
};

static const char* word(bool ok) { return ok ? "ok" : "FAIL"; }

// The two traffic legs: the peer produces while we read, then we write
// while the peer consumes. The service's half blocks in its handler, so
// it runs on a thread of ours.
static void traffic(const std::shared_ptr<IFmqPeer>& peer, Queue& q, int32_t count, bool* in,
                    bool* out) {
    int32_t produced = -1;
    std::thread producer([&] {
        if (!peer->produce(count, &produced).isOk()) produced = -1;
    });
    int64_t sum = 0;
    bool ordered = false;
    std::string why;
    bool readOk = readAll(q, count, &sum, &ordered, &why);
    if (!readOk) fprintf(stderr, "fmq_interop: local read: %s\n", why.c_str());
    producer.join();
    *in = readOk && ordered && sum == expectedSum(count) && produced == count;

    int64_t consumed = -1;
    std::thread consumer([&] {
        if (!peer->consume(count, &consumed).isOk()) consumed = -1;
    });
    bool writeOk = writeAll(q, count, &why);
    if (!writeOk) fprintf(stderr, "fmq_interop: local write: %s\n", why.c_str());
    consumer.join();
    *out = writeOk && consumed == expectedSum(count);
}

static Outcome serverQueue(const std::shared_ptr<IFmqPeer>& peer, int32_t count,
                           int32_t capacity) {
    Desc desc;
    ScopedAStatus st = peer->create(capacity, &desc);
    if (!st.isOk()) {
        return {"RESULT server-queue " + std::to_string(count) + " create=err:" + st.getDescription(),
                false};
    }
    std::string kind = memoryKind(desc);
    Queue q(desc);
    if (!q.isValid()) {
        return {"RESULT server-queue " + std::to_string(count) + " kind=" + kind + " attach=err:invalid",
                false};
    }
    bool in, out;
    traffic(peer, q, count, &in, &out);
    return {"RESULT server-queue " + std::to_string(count) + " kind=" + kind +
                    " capacity=" + std::to_string(q.getQuantumCount()) + " in=" + word(in) +
                    " out=" + word(out),
            q.getQuantumCount() == size_t(capacity) && in && out};
}

static Outcome clientQueue(const std::shared_ptr<IFmqPeer>& peer, int32_t count,
                           int32_t capacity) {
    Queue q(size_t(capacity), /*configureEventFlagWord=*/true);
    if (!q.isValid()) return {"RESULT client-queue " + std::to_string(count) + " create=err:invalid", false};
    int32_t adopted = -1;
    ScopedAStatus st = peer->adopt(q.dupeDesc(), &adopted);
    if (!st.isOk()) {
        return {"RESULT client-queue " + std::to_string(count) + " adopt=err:" + st.getDescription(),
                false};
    }
    bool in, out;
    traffic(peer, q, count, &in, &out);
    return {"RESULT client-queue " + std::to_string(count) + " adopted=" + std::to_string(adopted) +
                    " in=" + word(in) + " out=" + word(out),
            adopted == capacity && in && out};
}

// The queue's two counters through a mapping of our own.
struct Counters {
    void* base = MAP_FAILED;
    size_t len = 0;

    bool map(const Desc& desc) {
        int fd = desc.handle.fds[0].get();
        int size = ashmem_valid(fd) ? ashmem_get_size_region(fd) : -1;
        if (size < 0) {
            struct stat sb;
            if (fstat(fd, &sb) != 0) return false;
            size = int(sb.st_size);
        }
        len = size_t(size);
        base = mmap(nullptr, len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        return base != MAP_FAILED;
    }
    void set(uint64_t read, uint64_t write) {
        auto* p = static_cast<std::atomic<uint64_t>*>(base);
        p[0].store(read);
        p[1].store(write);
    }
    ~Counters() {
        if (base != MAP_FAILED) munmap(base, len);
    }
};

// S4 from this side: libfmq makes the queue, rsbinder adopts it, and the
// counters are damaged one way at a time; rsbinder's report is recorded.
static std::vector<Outcome> corrupt(const std::shared_ptr<IFmqPeer>& peer, int32_t capacity) {
    Queue q(size_t(capacity), true);
    if (!q.isValid()) return {{"RESULT corrupt create=err:invalid", false}};
    Desc desc = q.dupeDesc();
    int32_t adopted = -1;
    ScopedAStatus st = peer->adopt(desc, &adopted);
    if (!st.isOk()) return {{"RESULT corrupt adopt=err:" + st.getDescription(), false}};
    Counters counters;
    if (!counters.map(desc)) return {{"RESULT corrupt map=err:" + std::string(strerror(errno)), false}};
    uint64_t bytes = uint64_t(capacity) * 4;
    struct Damage {
        const char* what;
        uint64_t read, write;
    } damages[] = {{"read-ahead", 16, 0}, {"over-capacity", 0, bytes * 3}, {"unaligned", 0, 3}};
    std::vector<Outcome> out;
    for (const auto& d : damages) {
        counters.set(d.read, d.write);
        QueueView v;
        ScopedAStatus vs = peer->view(1, &v);
        if (vs.isOk()) {
            out.push_back({std::string("RESULT corrupt ") + d.what + " avail=" +
                                   std::to_string(v.availableToRead) + "/" +
                                   std::to_string(v.availableToWrite) +
                                   " read=" + std::to_string(v.readResult) +
                                   " sum=" + std::to_string(v.sum),
                           true});
        } else {
            out.push_back({std::string("RESULT corrupt ") + d.what + " view=err:" + vs.getDescription(),
                           false});
        }
    }
    return out;
}

static int runClient(const char* name, const std::string& what, int32_t count, int32_t capacity) {
    ndk::SpAIBinder binder(AServiceManager_checkService(name));
    if (binder.get() == nullptr) {
        printf("RESULT connect %s err:NameNotFound\n", name);
        return 1;
    }
    std::shared_ptr<IFmqPeer> peer = IFmqPeer::fromBinder(binder);
    if (!peer) {
        printf("RESULT connect %s err:BadType\n", name);
        return 1;
    }
    std::vector<Outcome> outcomes;
    if (what == "server-queue") {
        outcomes.push_back(serverQueue(peer, count, capacity));
    } else if (what == "client-queue") {
        outcomes.push_back(clientQueue(peer, count, capacity));
    } else if (what == "corrupt") {
        outcomes = corrupt(peer, capacity);
    } else if (what == "all") {
        outcomes.push_back(serverQueue(peer, count, capacity));
        outcomes.push_back(clientQueue(peer, count, capacity));
        for (auto& o : corrupt(peer, capacity)) outcomes.push_back(o);
    } else {
        return 2;
    }
    bool failed = false;
    for (const auto& o : outcomes) {
        printf("%s\n", o.line.c_str());
        failed |= !o.ok;
    }
    fflush(stdout);
    return failed ? 1 : 0;
}

// ---- what libfmq's headers and EventFlag.cpp want from libbase and
// libutils. The device's libfmq.so and libbase.so carry `std::__1`
// symbols, the NDK's libc++ is `std::__ndk1`, so these two libraries
// cannot be linked: EventFlag.cpp is compiled in, and its dependencies are
// these. The ring, counters and futex protocol are libfmq's code
// unchanged.

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
void check(bool exp, const char* message) {
    if (!exp) {
        __android_log_print(ANDROID_LOG_FATAL, "FMQ", "%s", message ? message : "check failed");
        abort();
    }
}
void logError(const std::string& m) { __android_log_print(ANDROID_LOG_ERROR, "FMQ", "%s", m.c_str()); }
void logWarning(const std::string& m) { __android_log_print(ANDROID_LOG_WARN, "FMQ", "%s", m.c_str()); }
void logDebug(const std::string& m) { __android_log_print(ANDROID_LOG_DEBUG, "FMQ", "%s", m.c_str()); }
void errorWriteLog(int tag, const char* info) {
    __android_log_print(ANDROID_LOG_ERROR, "FMQ", "%d: %s", tag, info ? info : "");
}
}  // namespace details
}  // namespace hardware
}  // namespace android

int main(int argc, char** argv) {
    if (argc >= 3 && strcmp(argv[1], "serve") == 0) return runServe(argv[2]);
    if (argc >= 4 && strcmp(argv[1], "client") == 0) {
        int32_t count = argc > 4 ? atoi(argv[4]) : kDefaultCount;
        int32_t capacity = argc > 5 ? atoi(argv[5]) : kDefaultCapacity;
        int rc = runClient(argv[2], argv[3], count, capacity);
        if (rc != 2) return rc;
    }
    fprintf(stderr,
            "usage: %s serve <name> | %s client <name> <server-queue|client-queue|corrupt|all> "
            "[count] [capacity]\n",
            argv[0], argv[0]);
    return 2;
}

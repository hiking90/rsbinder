// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// The other end of a queue, run by AOSP's own `MessageQueueBase` template
// (system/libfmq, unmodified) on a Linux host. Plan 12-fmq §6: it is the
// libfmq ring, counter and futex code that runs here; only the AIDL
// `MQDescriptor` parcelable is replaced, by the plain structs below and the
// socket record in `tests/common/mod.rs`, since there is no binder on the
// host. `tests/libfmq_host.rs` drives it.
//
//   libfmq_peer <create|attach> <read|write> <socket path> <count> <capacity>
//
// It connects to the socket. `create` makes the queue with libfmq (through
// the host `ashmem_create_region`, a sealed memfd) and sends the descriptor;
// `attach` receives one and attaches with libfmq's default
// `resetPointers = true`. Either way it then sends one byte ("attached")
// and, as writer, `writeBlocking`s 0..count in chunks and waits for a byte
// before exiting (so the mapping outlives the reader's last read); as
// reader, `readBlocking`s count items, checks the sequence and prints
// `sum=<total>`.

// The libfmq headers expect these to have been included already.
#include <algorithm>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <climits>
#include <cstring>
#include <memory>
#include <string>
#include <vector>

#include <cutils/native_handle.h>
#include <fmq/MQDescriptorBase.h>
#include <fmq/MessageQueueBase.h>
#include <fmq/AidlMQDescriptorShimBase.h>

#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

using android::hardware::GrantorDescriptor;
using android::hardware::kSynchronizedReadWrite;
using android::hardware::MQFlavor;

// ---- the AIDL types libfmq's shim is written against, as plain structs ----

struct HostFd {
    int fd_ = -1;
    HostFd() = default;
    explicit HostFd(int fd) : fd_(fd) {}
    int get() const { return fd_; }
};

struct HostGrantor {
    int32_t fdIndex;
    int32_t offset;
    int64_t extent;
};

struct HostHandle {
    std::vector<HostFd> fds;
    std::vector<int32_t> ints;
};

template <typename T, typename Flavor>
struct HostMQDescriptor {
    std::vector<HostGrantor> grantors;
    HostHandle handle;
    int32_t quantum;
    int32_t flags;
};

struct SyncType {};
struct UnsyncType {};

struct HostBackend {
    template <typename T, typename F>
    using MQDescriptorType = HostMQDescriptor<T, F>;
    using SynchronizedReadWriteType = SyncType;
    using UnsynchronizedWriteType = UnsyncType;
};

template <typename T, MQFlavor flavor>
struct HostShim : public android::details::AidlMQDescriptorShimBase<T, flavor, HostBackend> {
    using Base = android::details::AidlMQDescriptorShimBase<T, flavor, HostBackend>;
    HostShim(const std::vector<GrantorDescriptor>& grantors, native_handle_t* handle, size_t size)
        : Base(grantors, handle, size) {}
    HostShim(size_t bufferSize, native_handle_t* handle, size_t messageSize,
             bool configureEventFlag = false)
        : Base(bufferSize, handle, messageSize, configureEventFlag) {}
    explicit HostShim(const HostShim& other) : Base(other) {}
};

using Queue = android::MessageQueueBase<HostShim, uint32_t, kSynchronizedReadWrite>;

// ---- socket plumbing ------------------------------------------------------

static int connect_unix(const char* path) {
    int fd = socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (fd < 0) { perror("socket"); exit(2); }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    for (int i = 0; i < 2000; i++) {
        if (connect(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) == 0) return fd;
        usleep(5000);
    }
    perror("connect");
    exit(2);
}

static void put_u32(std::vector<uint8_t>& v, uint32_t x) {
    for (int i = 0; i < 4; i++) v.push_back(uint8_t(x >> (8 * i)));
}
static void put_u64(std::vector<uint8_t>& v, uint64_t x) {
    for (int i = 0; i < 8; i++) v.push_back(uint8_t(x >> (8 * i)));
}
static uint32_t get_u32(const uint8_t* p) {
    return uint32_t(p[0]) | uint32_t(p[1]) << 8 | uint32_t(p[2]) << 16 | uint32_t(p[3]) << 24;
}
static uint64_t get_u64(const uint8_t* p) {
    return uint64_t(get_u32(p)) | uint64_t(get_u32(p + 4)) << 32;
}

static void send_descriptor(int sock, const Queue& q) {
    const auto* desc = q.getDesc();
    const native_handle_t* h = desc->handle();
    std::vector<uint8_t> body;
    put_u32(body, uint32_t(desc->getQuantum()));
    put_u32(body, desc->getFlags());
    put_u32(body, uint32_t(desc->grantors().size()));
    put_u32(body, uint32_t(h->numInts));
    for (const auto& g : desc->grantors()) {
        put_u32(body, g.fdIndex);
        put_u32(body, g.offset);
        put_u64(body, g.extent);
    }
    for (int i = 0; i < h->numInts; i++) put_u32(body, uint32_t(h->data[h->numFds + i]));
    std::vector<uint8_t> msg;
    put_u32(msg, uint32_t(body.size()));
    msg.insert(msg.end(), body.begin(), body.end());

    std::vector<uint8_t> control(CMSG_SPACE(sizeof(int) * h->numFds));
    iovec iov{msg.data(), msg.size()};
    msghdr mh{};
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = control.data();
    mh.msg_controllen = control.size();
    cmsghdr* c = CMSG_FIRSTHDR(&mh);
    c->cmsg_level = SOL_SOCKET;
    c->cmsg_type = SCM_RIGHTS;
    c->cmsg_len = CMSG_LEN(sizeof(int) * h->numFds);
    memcpy(CMSG_DATA(c), h->data, sizeof(int) * h->numFds);
    if (sendmsg(sock, &mh, 0) != ssize_t(msg.size())) { perror("sendmsg"); exit(2); }
}

// Returns a shim that owns the received fds.
static HostShim<uint32_t, kSynchronizedReadWrite>* recv_descriptor(int sock) {
    std::vector<uint8_t> data(4096);
    std::vector<uint8_t> control(CMSG_SPACE(sizeof(int) * 8));
    iovec iov{data.data(), data.size()};
    msghdr mh{};
    mh.msg_iov = &iov;
    mh.msg_iovlen = 1;
    mh.msg_control = control.data();
    mh.msg_controllen = control.size();
    ssize_t n = recvmsg(sock, &mh, 0);
    if (n < 20) { perror("recvmsg"); exit(2); }
    std::vector<int> fds;
    for (cmsghdr* c = CMSG_FIRSTHDR(&mh); c; c = CMSG_NXTHDR(&mh, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) {
            size_t count = (c->cmsg_len - CMSG_LEN(0)) / sizeof(int);
            for (size_t i = 0; i < count; i++) {
                int fd;
                memcpy(&fd, CMSG_DATA(c) + i * sizeof(int), sizeof(int));
                fds.push_back(fd);
            }
        }
    }
    const uint8_t* p = data.data();
    uint32_t len = get_u32(p);
    if (len + 4 != uint32_t(n)) { fprintf(stderr, "record length %u vs %zd\n", len, n); exit(2); }
    uint32_t quantum = get_u32(p + 4);
    uint32_t flags = get_u32(p + 8);
    uint32_t n_grantors = get_u32(p + 12);
    uint32_t n_ints = get_u32(p + 16);
    if (flags != kSynchronizedReadWrite) { fprintf(stderr, "flavor %u\n", flags); exit(2); }
    size_t o = 20;
    std::vector<GrantorDescriptor> grantors;
    for (uint32_t i = 0; i < n_grantors; i++) {
        GrantorDescriptor g;
        g.flags = 0;
        g.fdIndex = get_u32(p + o);
        g.offset = get_u32(p + o + 4);
        g.extent = get_u64(p + o + 8);
        grantors.push_back(g);
        o += 16;
    }
    native_handle_t* h = native_handle_create(int(fds.size()), int(n_ints));
    for (size_t i = 0; i < fds.size(); i++) h->data[i] = fds[i];
    for (uint32_t i = 0; i < n_ints; i++) {
        h->data[fds.size() + i] = int(get_u32(p + o));
        o += 4;
    }
    return new HostShim<uint32_t, kSynchronizedReadWrite>(grantors, h, quantum);
}

static void send_byte(int sock) {
    uint8_t b = 1;
    if (write(sock, &b, 1) != 1) { perror("write"); exit(2); }
}

static void recv_byte(int sock) {
    uint8_t b;
    if (read(sock, &b, 1) != 1) { perror("read"); exit(2); }
}

// ---- roles ----------------------------------------------------------------

static const int64_t kTimeoutNs = 10LL * 1000 * 1000 * 1000;

static void run_writer(Queue& q, uint32_t count) {
    uint32_t next = 0;
    uint32_t chunk[5];
    while (next < count) {
        size_t n = std::min<size_t>(5, count - next);
        for (size_t i = 0; i < n; i++) chunk[i] = next + uint32_t(i);
        if (!q.writeBlocking(chunk, n, kTimeoutNs)) {
            fprintf(stderr, "writeBlocking failed at %u\n", next);
            exit(1);
        }
        next += uint32_t(n);
    }
}

static void run_reader(Queue& q, uint32_t count) {
    uint32_t next = 0;
    uint64_t sum = 0;
    uint32_t chunk[7];
    while (next < count) {
        size_t n = std::min<size_t>(7, count - next);
        if (!q.readBlocking(chunk, n, kTimeoutNs)) {
            fprintf(stderr, "readBlocking failed at %u\n", next);
            exit(1);
        }
        for (size_t i = 0; i < n; i++) {
            if (chunk[i] != next) {
                fprintf(stderr, "item %u: got %u\n", next, chunk[i]);
                exit(1);
            }
            sum += chunk[i];
            next++;
        }
    }
    printf("sum=%llu\n", static_cast<unsigned long long>(sum));
    fflush(stdout);
}

int main(int argc, char** argv) {
    if (argc != 6) {
        fprintf(stderr, "usage: %s <create|attach> <read|write> <socket> <count> <capacity>\n", argv[0]);
        return 2;
    }
    std::string origin = argv[1], role = argv[2];
    uint32_t count = uint32_t(strtoul(argv[4], nullptr, 10));
    size_t capacity = strtoul(argv[5], nullptr, 10);
    int sock = connect_unix(argv[3]);

    Queue* q;
    if (origin == "create") {
        q = new Queue(capacity, /*configureEventFlagWord=*/true);
        if (!q->isValid()) { fprintf(stderr, "create: invalid queue\n"); return 1; }
        send_descriptor(sock, *q);
    } else {
        auto* shim = recv_descriptor(sock);
        q = new Queue(*shim /* resetPointers = true, libfmq's default */);
        if (!q->isValid()) { fprintf(stderr, "attach: invalid queue\n"); return 1; }
        if (q->getQuantumCount() != capacity) {
            fprintf(stderr, "attach: capacity %zu, expected %zu\n", q->getQuantumCount(), capacity);
            return 1;
        }
    }
    printf("capacity=%zu grantors=%zu\n", q->getQuantumCount(), q->getDesc()->countGrantors());
    fflush(stdout);
    send_byte(sock);  // attached (or created and described)

    if (role == "write") {
        recv_byte(sock);  // the peer has attached; libfmq's attach resets the counters
        run_writer(*q, count);
        recv_byte(sock);  // the peer has read everything
        printf("done\n");
    } else {
        run_reader(*q, count);
    }
    delete q;
    return 0;
}

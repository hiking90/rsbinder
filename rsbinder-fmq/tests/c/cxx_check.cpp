// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// c/rsbinder_fmq.h compiled as C++, with its two templates instantiated on
// a stand-in for the NDK backend's MQDescriptor (the fields the generated
// type has: handle.fds of ScopedFileDescriptor, handle.ints, grantors,
// quantum, flags). Makes a queue, converts it out and back, attaches.
// Prints "ok" and exits 0 on success.

#include "rsbinder_fmq.h"

#include <cstdio>
#include <vector>

namespace {

struct ScopedFd {
    int fd = -1;
    explicit ScopedFd(int f) : fd(f) {}
    ScopedFd(ScopedFd &&o) noexcept : fd(o.fd) { o.fd = -1; }
    ScopedFd(const ScopedFd &) = delete;
    ~ScopedFd() {
        if (fd >= 0) close(fd);
    }
    int get() const { return fd; }
};

struct Grantor {
    int32_t fdIndex = 0;
    int32_t offset = 0;
    int64_t extent = 0;
};

struct NativeHandle {
    std::vector<ScopedFd> fds;
    std::vector<int32_t> ints;
};

struct MQDescriptor {
    std::vector<Grantor> grantors;
    NativeHandle handle;
    int32_t quantum = 0;
    int32_t flags = 0;
};

}  // namespace

int main() {
    rsbfmq_queue made;
    if (rsbfmq_create(&made, 64, 4, 1) != 0) return 1;
    MQDescriptor mqd;
    if (rsbfmq_desc_to_aidl(made, &mqd) != 0) return 2;
    if (mqd.grantors.size() != 4 || mqd.quantum != 4 || mqd.flags != 1) return 3;

    std::vector<int> fds;
    std::vector<rsbfmq_grantor> grantors;
    rsbfmq_desc d;
    if (rsbfmq_desc_from_aidl(mqd, fds, grantors, &d) != 0) return 4;
    rsbfmq_policy p{64, 1, 1};
    rsbfmq_queue attached;
    if (rsbfmq_attach(&attached, &d, 4, &p) != 0) return 5;

    uint32_t in = 42, out = 0;
    if (rsbfmq_write(&made, &in, 1) != 0 || rsbfmq_read(&attached, &out, 1) != 0 || out != 42) {
        return 6;
    }
    mqd.grantors[0].offset = -8;
    if (rsbfmq_desc_from_aidl(mqd, fds, grantors, &d) != -EINVAL) return 7;

    rsbfmq_close(&attached);
    rsbfmq_close(&made);
    std::printf("ok\n");
    return 0;
}

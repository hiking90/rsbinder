// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// What libfmq needs from libcutils, libutils, liblog and libbase, on a Linux
// host without them. Each replacement does what the Android original does
// where the queue can tell, and nothing where it cannot:
//
//  * `ashmem_create_region` — libcutils on Android 11+ (`ashmem-dev.cpp`,
//    `use_memfd()`) makes a memfd sealed `F_SEAL_GROW | F_SEAL_SHRINK`;
//    that is what the rsbinder attach check sees, so it is what this does.
//  * `ashmem_set_prot_region(PROT_READ | PROT_WRITE)` — a no-op on the
//    memfd path too.
//  * `elapsedRealtimeNano` — `CLOCK_BOOTTIME`, as libutils.
//  * logging — stderr.

#include <cutils/ashmem.h>
#include <utils/SystemClock.h>
#include <log/log.h>

#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <string>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>

extern "C" int ashmem_create_region(const char* name, size_t size) {
    int fd = memfd_create(name, MFD_CLOEXEC | MFD_ALLOW_SEALING);
    if (fd < 0) return -1;
    if (ftruncate(fd, size) < 0 || fcntl(fd, F_ADD_SEALS, F_SEAL_GROW | F_SEAL_SHRINK) < 0) {
        close(fd);
        return -1;
    }
    return fd;
}

extern "C" int ashmem_set_prot_region(int, int) {
    return 0;
}

extern "C" int ashmem_valid(int) {
    return 1;
}

namespace android {
int64_t elapsedRealtimeNano() {
    struct timespec ts;
    clock_gettime(CLOCK_BOOTTIME, &ts);
    return int64_t(ts.tv_sec) * 1000000000LL + ts.tv_nsec;
}
}  // namespace android

extern "C" int __android_log_print(int, const char* tag, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "[%s] ", tag ? tag : "");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return 0;
}

extern "C" int __android_log_buf_print(int, int, const char* tag, const char* fmt, ...) {
    va_list ap;
    va_start(ap, fmt);
    fprintf(stderr, "[%s] ", tag ? tag : "");
    vfprintf(stderr, fmt, ap);
    fputc('\n', stderr);
    va_end(ap);
    return 0;
}

extern "C" int __android_log_is_loggable(int, const char*, int) {
    return 1;
}

extern "C" void __android_log_assert(const char* cond, const char* tag, const char* fmt, ...) {
    fprintf(stderr, "[%s] assertion failed: %s\n", tag ? tag : "", cond ? cond : "");
    if (fmt) {
        va_list ap;
        va_start(ap, fmt);
        vfprintf(stderr, fmt, ap);
        fputc('\n', stderr);
        va_end(ap);
    }
    abort();
}

// FmqInternal.cpp, without libbase's `LOG`/`CHECK`.
namespace android {
namespace hardware {
namespace details {

void check(bool exp) {
    if (!exp) {
        fprintf(stderr, "[FMQ] check failed\n");
        abort();
    }
}

void check(bool exp, const char* message) {
    if (!exp) {
        fprintf(stderr, "[FMQ] check failed: %s\n", message);
        abort();
    }
}

void logError(const std::string& message) {
    fprintf(stderr, "[FMQ] error: %s\n", message.c_str());
}

void logWarning(const std::string& message) {
    fprintf(stderr, "[FMQ] warning: %s\n", message.c_str());
}

void logDebug(const std::string& message) {
    fprintf(stderr, "[FMQ] debug: %s\n", message.c_str());
}

void errorWriteLog(int tag, const char* info) {
    fprintf(stderr, "[FMQ] errorWriteLog(%#x): %s\n", tag, info ? info : "");
}

}  // namespace details
}  // namespace hardware
}  // namespace android

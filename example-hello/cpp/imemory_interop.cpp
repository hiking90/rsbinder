// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 4-7a E5 STAGE3 — real libbinder `android::IMemory` /
// `android::IMemoryHeap` against rsbinder, over the kernel binder and
// Android's servicemanager. Two modes:
//
//   imemory_interop client
//     getService("rsbinder.test.shm") (served by rsbinder `test_service`)
//     → interface_cast<IMemory> → getMemory() → unsecurePointer(): read
//     the pattern rsbinder wrote, write a marker back for rsbinder to
//     verify, print STAGE3_4_7A_CLIENT_PASS.
//
//   imemory_interop server
//     MemoryHeapBase + MemoryBase, addService("rsbinder.test.cppshm"),
//     joinThreadPool. rsbinder's `BpMemory` is the client.
//
//   imemory_interop client-large <service-name> <bytes>
//   imemory_interop server-large <service-name> <bytes>
//     The same two roles over a heap far above the 4 MB transaction
//     limit, paired with `tests/src/bin/shm_probe.rs`: the window covers
//     the whole heap, filled with `i % 251`. `client-large` checks every
//     byte, prints `RESULT cpp-shm <bytes> OK <checksum>`, then writes
//     `cpp-large-tail` into the last 16 bytes for rsbinder to read back.
//
// Built with the NDK clang against AOSP libbinder headers and the
// emulator's own libbinder.so (see run_imemory_interop.sh).

#include <binder/IMemory.h>
#include <binder/IPCThreadState.h>
#include <binder/IServiceManager.h>
#include <binder/MemoryBase.h>
#include <binder/MemoryHeapBase.h>
#include <binder/ProcessState.h>
#include <unistd.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

using namespace android;

static const char* kRustService = "rsbinder.test.shm";
static const char* kCppService = "rsbinder.test.cppshm";
static const char* kRustPattern = "kernel-shm-from-server";
static const char* kCppClientMarker = "cpp-client-wrote";
static const char* kCppServerPattern = "cpp-server-pattern";

static int runClient() {
    sp<IServiceManager> sm = defaultServiceManager();
    sp<IBinder> binder = sm->checkService(String16(kRustService));
    if (binder == nullptr) {
        fprintf(stderr, "FAIL: %s not found\n", kRustService);
        return 1;
    }
    sp<IMemory> mem = interface_cast<IMemory>(binder);
    if (mem == nullptr) {
        fprintf(stderr, "FAIL: interface_cast<IMemory> failed\n");
        return 1;
    }
    ssize_t offset = -1;
    size_t size = 0;
    sp<IMemoryHeap> heap = mem->getMemory(&offset, &size);
    if (heap == nullptr) {
        fprintf(stderr, "FAIL: getMemory returned null heap\n");
        return 1;
    }
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    printf("heap: id=%d size=%zu flags=%#x offset=%jd; window offset=%zd size=%zu\n",
           heap->getHeapID(), heap->getSize(), heap->getFlags(), (intmax_t)heap->getOffset(),
           offset, size);
    if (heap->getSize() != page * 2 || (size_t)offset != page || size != page) {
        fprintf(stderr, "FAIL: unexpected geometry\n");
        return 1;
    }
    char* base = static_cast<char*>(mem->unsecurePointer());
    if (base == nullptr) {
        fprintf(stderr, "FAIL: unsecurePointer null\n");
        return 1;
    }
    if (strncmp(base, kRustPattern, strlen(kRustPattern)) != 0) {
        fprintf(stderr, "FAIL: pattern mismatch: '%.*s'\n", (int)strlen(kRustPattern), base);
        return 1;
    }
    memcpy(base + 128, kCppClientMarker, strlen(kCppClientMarker) + 1);
    printf("STAGE3_4_7A_CLIENT_PASS\n");
    return 0;
}

static int runServer() {
    const size_t page = (size_t)sysconf(_SC_PAGESIZE);
    sp<MemoryHeapBase> heap = new MemoryHeapBase(page * 2, 0, "rsbinder-stage3");
    if (heap->getHeapID() < 0 || heap->getBase() == MAP_FAILED) {
        fprintf(stderr, "FAIL: MemoryHeapBase allocation failed\n");
        return 1;
    }
    char* base = static_cast<char*>(heap->getBase());
    memcpy(base + page, kCppServerPattern, strlen(kCppServerPattern) + 1);
    sp<MemoryBase> mem = new MemoryBase(heap, page, page);
    status_t st = defaultServiceManager()->addService(String16(kCppService), mem);
    if (st != OK) {
        fprintf(stderr, "FAIL: addService(%s) = %d\n", kCppService, st);
        return 1;
    }
    printf("STAGE3_4_7A_SERVER_READY\n");
    fflush(stdout);
    ProcessState::self()->startThreadPool();
    IPCThreadState::self()->joinThreadPool();
    return 0;
}

static const size_t kTailLen = 16;
static const char kCppTail[kTailLen] = "cpp-large-tail";

static uint8_t byteAt(size_t i) { return static_cast<uint8_t>(i % 251); }

static int runClientLarge(const char* name, size_t bytes) {
    sp<IBinder> binder = defaultServiceManager()->checkService(String16(name));
    if (binder == nullptr) {
        printf("RESULT cpp-shm %zu ERROR not-found\n", bytes);
        return 1;
    }
    sp<IMemory> mem = interface_cast<IMemory>(binder);
    ssize_t offset = -1;
    size_t size = 0;
    sp<IMemoryHeap> heap = mem == nullptr ? nullptr : mem->getMemory(&offset, &size);
    if (heap == nullptr || heap->getSize() < bytes || offset != 0 || size != bytes) {
        printf("RESULT cpp-shm %zu ERROR geometry heap=%zu offset=%zd size=%zu\n", bytes,
               heap == nullptr ? 0 : heap->getSize(), offset, size);
        return 1;
    }
    uint8_t* base = static_cast<uint8_t*>(mem->unsecurePointer());
    if (base == nullptr) {
        printf("RESULT cpp-shm %zu ERROR unmapped\n", bytes);
        return 1;
    }
    uint32_t sum = 0;
    for (size_t i = 0; i < bytes; i++) {
        if (base[i] != byteAt(i)) {
            printf("RESULT cpp-shm %zu ERROR payload-mismatch at %zu\n", bytes, i);
            return 1;
        }
        sum += static_cast<uint32_t>(i) ^ static_cast<uint32_t>(base[i]);
    }
    memcpy(base + bytes - kTailLen, kCppTail, kTailLen);
    printf("RESULT cpp-shm %zu OK %u\n", bytes, sum);
    return 0;
}

static int runServerLarge(const char* name, size_t bytes) {
    sp<MemoryHeapBase> heap = new MemoryHeapBase(bytes, 0, "rsbinder-stage3-large");
    if (heap->getHeapID() < 0 || heap->getBase() == MAP_FAILED) {
        fprintf(stderr, "FAIL: MemoryHeapBase(%zu) allocation failed\n", bytes);
        return 1;
    }
    uint8_t* base = static_cast<uint8_t*>(heap->getBase());
    for (size_t i = 0; i < bytes; i++) base[i] = byteAt(i);    sp<MemoryBase> mem = new MemoryBase(heap, 0, bytes);
    status_t st = defaultServiceManager()->addService(String16(name), mem);
    if (st != OK) {
        fprintf(stderr, "FAIL: addService(%s) = %d\n", name, st);
        return 1;
    }
    printf("SERVING %s %zu\n", name, bytes);
    fflush(stdout);
    ProcessState::self()->startThreadPool();
    IPCThreadState::self()->joinThreadPool();
    return 0;
}

int main(int argc, char** argv) {
    if (argc == 4) {
        std::string mode = argv[1];
        size_t bytes = strtoull(argv[3], nullptr, 10);
        if (bytes < kTailLen) {
            fprintf(stderr, "bytes must be at least %zu\n", kTailLen);
            return 2;
        }
        if (mode == "client-large") return runClientLarge(argv[2], bytes);
        if (mode == "server-large") return runServerLarge(argv[2], bytes);
    }
    if (argc != 2) {
        fprintf(stderr, "usage: %s client|server | client-large|server-large <name> <bytes>\n",
                argv[0]);
        return 2;
    }
    std::string mode = argv[1];
    if (mode == "client") return runClient();
    if (mode == "server") return runServer();
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
}

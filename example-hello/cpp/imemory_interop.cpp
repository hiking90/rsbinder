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
// Built with the NDK clang against AOSP libbinder headers and the
// emulator's own libbinder.so (see run_imemory_interop.sh).

#include <binder/IMemory.h>
#include <binder/IPCThreadState.h>
#include <binder/IServiceManager.h>
#include <binder/MemoryBase.h>
#include <binder/MemoryHeapBase.h>
#include <binder/ProcessState.h>
#include <unistd.h>

#include <cstdio>
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

int main(int argc, char** argv) {
    if (argc != 2) {
        fprintf(stderr, "usage: %s client|server\n", argv[0]);
        return 2;
    }
    std::string mode = argv[1];
    if (mode == "client") return runClient();
    if (mode == "server") return runServer();
    fprintf(stderr, "unknown mode %s\n", argv[1]);
    return 2;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-3 AC-3.2 STAGE3: a pipe fd in a parcel between rsbinder and a
// real AOSP binder peer, both directions.
//
// What needs a real peer here is the `ParcelFileDescriptor` wire —
// `int32` not-null marker, `int32 hasComm`, then the descriptor object
// (AOSP `Parcel::writeParcelFileDescriptor`) — and the descriptor
// translation the driver does on the way. The NDK reads and writes
// exactly that form through `AParcel_{read,write}ParcelFileDescriptor`,
// so unlike the blob harness this one covers the whole mechanism.
//
//   pipe_interop client <service-name> <bytes>
//   pipe_interop server <service-name>
//
// Shares its contract with `tests/src/bin/pipe_probe.rs`: descriptor
// "rsbinder.test.pipe.IProbe", code 1 = OPEN (request: int32 length;
// reply: a ParcelFileDescriptor being filled with `i % 251`).

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>
#include <unistd.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <thread>
#include <vector>

extern "C" {
AIBinder* AServiceManager_checkService(const char* instance);
binder_status_t AServiceManager_addService(AIBinder* binder, const char* instance);
void ABinderProcess_startThreadPool(void);
void ABinderProcess_joinThreadPool(void);
}

static constexpr const char* kDescriptor = "rsbinder.test.pipe.IProbe";
static constexpr transaction_code_t kOpen = FIRST_CALL_TRANSACTION;

static uint8_t byte_at(size_t i) { return static_cast<uint8_t>(i % 251); }

static uint32_t checksum(const std::vector<uint8_t>& bytes) {
    uint32_t acc = 0;
    for (size_t i = 0; i < bytes.size(); i++)
        acc += static_cast<uint32_t>(i) ^ static_cast<uint32_t>(bytes[i]);
    return acc;
}

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}

/// Fill `fd` with `len` bytes of the shared pattern, then close it. Runs
/// detached: a pipe holds about 64 KB, so this must not be on the thread
/// answering the transaction — the caller has not received the read end
/// yet and cannot drain.
static void fill_pipe(int fd, size_t len) {
    std::thread([fd, len]() {
        size_t written = 0;
        while (written < len) {
            size_t take = std::min<size_t>(64 * 1024, len - written);
            std::vector<uint8_t> piece(take);
            for (size_t i = 0; i < take; i++) piece[i] = byte_at(written + i);
            size_t off = 0;
            while (off < take) {
                ssize_t n = write(fd, piece.data() + off, take - off);
                if (n <= 0) {
                    close(fd);
                    return;
                }
                off += static_cast<size_t>(n);
            }
            written += take;
        }
        close(fd);
    }).detach();
}

// --- server role -----------------------------------------------------

static binder_status_t on_transact(AIBinder*, transaction_code_t code, const AParcel* in,
                                   AParcel* out) {
    if (code != kOpen) return STATUS_UNKNOWN_TRANSACTION;
    int32_t len = 0;
    binder_status_t st = AParcel_readInt32(in, &len);
    if (st != STATUS_OK || len < 0) return STATUS_BAD_VALUE;

    int fds[2];
    if (pipe(fds) != 0) return STATUS_NO_MEMORY;
    fprintf(stderr, "[pipe_interop] streaming %d bytes\n", len);
    fill_pipe(fds[1], static_cast<size_t>(len));
    st = AParcel_writeParcelFileDescriptor(out, fds[0]);
    close(fds[0]);  // the parcel dup'd it
    return st;
}

static int run_server(const char* name) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    AIBinder* binder = AIBinder_new(clazz, nullptr);
    if (!binder) {
        fprintf(stderr, "AIBinder_new failed\n");
        return 2;
    }
    binder_status_t st = AServiceManager_addService(binder, name);
    if (st != STATUS_OK) {
        fprintf(stderr, "AServiceManager_addService(%s) = %d\n", name, st);
        return 3;
    }
    printf("SERVING %s\n", name);
    fflush(stdout);
    ABinderProcess_startThreadPool();
    ABinderProcess_joinThreadPool();
    return 0;
}

// --- client role -----------------------------------------------------

static int run_client(const char* name, size_t len) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    AIBinder* binder = AServiceManager_checkService(name);
    if (!binder) {
        printf("RESULT cpp %zu ERROR not-registered\n", len);
        return 1;
    }
    if (!AIBinder_associateClass(binder, clazz)) {
        printf("RESULT cpp %zu ERROR descriptor-mismatch\n", len);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st == STATUS_OK) st = AParcel_writeInt32(in, static_cast<int32_t>(len));
    if (st != STATUS_OK) {
        printf("RESULT cpp %zu ERROR prepare=%d\n", len, st);
        AIBinder_decStrong(binder);
        return 1;
    }
    AParcel* out = nullptr;
    st = AIBinder_transact(binder, kOpen, &in, &out, 0);
    if (st != STATUS_OK) {
        printf("RESULT cpp %zu ERROR transact=%d\n", len, st);
        AIBinder_decStrong(binder);
        return 1;
    }

    int fd = -1;
    st = AParcel_readParcelFileDescriptor(out, &fd);
    AParcel_delete(out);
    AIBinder_decStrong(binder);
    if (st != STATUS_OK || fd < 0) {
        printf("RESULT cpp %zu ERROR read-fd=%d\n", len, st);
        return 1;
    }

    std::vector<uint8_t> got;
    got.reserve(len);
    std::vector<uint8_t> buf(64 * 1024);
    for (;;) {
        ssize_t n = read(fd, buf.data(), buf.size());
        if (n < 0) {
            printf("RESULT cpp %zu ERROR read=%d\n", len, errno);
            close(fd);
            return 1;
        }
        if (n == 0) break;
        got.insert(got.end(), buf.begin(), buf.begin() + n);
    }
    close(fd);

    if (got.size() != len) {
        printf("RESULT cpp %zu ERROR short=%zu\n", len, got.size());
        return 1;
    }
    for (size_t i = 0; i < len; i++) {
        if (got[i] != byte_at(i)) {
            printf("RESULT cpp %zu ERROR payload-mismatch\n", len);
            return 1;
        }
    }
    printf("RESULT cpp %zu OK %u\n", len, checksum(got));
    return 0;
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) return run_server(argv[2]);
    if (argc == 4 && strcmp(argv[1], "client") == 0) {
        return run_client(argv[2], strtoul(argv[3], nullptr, 10));
    }
    fprintf(stderr, "usage: %s client <service-name> <bytes> | server <service-name>\n", argv[0]);
    return 2;
}

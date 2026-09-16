// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-1 AC-1.5 STAGE3: a real AOSP binder client sends a payload
// larger than the default receive mapping to an rsbinder service that
// raised its own (`binder://?mmap=`).
//
// What this proves that a rsbinder-to-rsbinder test cannot: the mapping
// is the *receiver's* property and nothing about it is negotiated, so a
// stock AOSP sender — which knows only `BINDER_VM_SIZE` for itself —
// must reach a larger rsbinder service with no change on its side. If
// anything about the size had to be agreed, this client could not have
// been written without touching it.
//
//   mmap_size_interop <service-name> <payload-bytes>
//
// Prints one line, mirroring `tests/src/bin/mmap_probe.rs` so the
// driving script greps for the same shapes:
//
//   RESULT call <bytes> OK
//   RESULT call <bytes> FAILEDTXN
//   RESULT call <bytes> ERROR <detail>
//
// Exit 0 on OK, 1 otherwise.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <cstdio>
#include <cstdlib>
#include <string>
#include <vector>

// `binder_manager.h` and the `AServiceManager_*` entry points are
// platform-only headers; the NDK sysroot ships neither, although every
// device image has the symbols in `/system/lib64/libbinder_ndk.so`. Same
// declaration-by-hand the other STAGE3 launchers here use.
extern "C" AIBinder* AServiceManager_checkService(const char* instance);

// Must match `tests/src/bin/mmap_probe.rs`.
static constexpr const char* kDescriptor = "rsbinder.test.mmap.IProbe";
static constexpr transaction_code_t kConsume = FIRST_CALL_TRANSACTION;

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}
static binder_status_t on_transact(AIBinder*, transaction_code_t, const AParcel*,
                                   AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

int main(int argc, char** argv) {
    if (argc != 3) {
        fprintf(stderr, "usage: %s <service-name> <payload-bytes>\n", argv[0]);
        return 2;
    }
    const char* name = argv[1];
    const size_t bytes = strtoul(argv[2], nullptr, 10);

    AIBinder_Class* clazz =
        AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    if (!clazz) {
        printf("RESULT call %zu ERROR class-define\n", bytes);
        return 1;
    }

    AIBinder* binder = AServiceManager_checkService(name);
    if (!binder) {
        printf("RESULT call %zu ERROR not-registered\n", bytes);
        return 1;
    }
    // Also the descriptor check: libbinder asks the binder for its
    // interface descriptor and compares, so a mismatch fails here rather
    // than inside the transaction.
    if (!AIBinder_associateClass(binder, clazz)) {
        printf("RESULT call %zu ERROR descriptor-mismatch\n", bytes);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st != STATUS_OK) {
        printf("RESULT call %zu ERROR prepare=%d\n", bytes, st);
        AIBinder_decStrong(binder);
        return 1;
    }
    // `int8_t` because that is what AIDL's `byte[]` is on this side.
    std::vector<int8_t> payload(bytes, static_cast<int8_t>(0xa5));
    st = AParcel_writeByteArray(in, payload.data(),
                                static_cast<int32_t>(payload.size()));
    if (st != STATUS_OK) {
        printf("RESULT call %zu ERROR write=%d\n", bytes, st);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* out = nullptr;
    st = AIBinder_transact(binder, kConsume, &in, &out, 0);
    int rc = 1;
    if (st == STATUS_FAILED_TRANSACTION) {
        // What a payload too large for the receiver's mapping looks like
        // from here: the driver refuses it, the service never runs.
        printf("RESULT call %zu FAILEDTXN\n", bytes);
    } else if (st != STATUS_OK) {
        printf("RESULT call %zu ERROR transact=%d\n", bytes, st);
    } else {
        int32_t echoed = -1;
        binder_status_t rst = AParcel_readInt32(out, &echoed);
        if (rst != STATUS_OK) {
            printf("RESULT call %zu ERROR read=%d\n", bytes, rst);
        } else if (static_cast<size_t>(echoed) != bytes) {
            printf("RESULT call %zu ERROR echoed=%d\n", bytes, echoed);
        } else {
            printf("RESULT call %zu OK\n", bytes);
            rc = 0;
        }
    }
    if (out) AParcel_delete(out);
    AIBinder_decStrong(binder);
    return rc;
}

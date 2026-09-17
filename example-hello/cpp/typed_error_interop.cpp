// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-4 AC-4.5 STAGE3: a service-specific error crossing between
// rsbinder and the real AOSP binder stack, in both directions.
//
// 10-4 adds no bytes to the wire — it is a Rust-side mapping over the
// `Status` header AOSP already defines. That is exactly why it needs a
// real peer to confirm: hermetic tests have rsbinder writing the header
// and rsbinder reading it back, so a shared misreading of the format
// would pass them both.
//
//   typed_error_interop client <service-name> <code>
//   typed_error_interop server <service-name>
//
// `client` calls an rsbinder service and prints what libbinder made of
// the failure; `server` publishes a service that throws, for an rsbinder
// client to read. Both print one line:
//
//   RESULT cpp <code> SERVICE_SPECIFIC <code> <message>
//   RESULT cpp <code> ERROR <detail>
//   SERVING <name>                                   (server role)
//
// Linked against the device's own libbinder_ndk.so: the NDK sysroot has
// the parcel/status/ibinder headers, and `AServiceManager_*` is declared
// by hand below (platform-only header, but the symbols ship on every
// device image). Same technique as the other STAGE3 launchers here.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <string>

extern "C" {
AIBinder* AServiceManager_checkService(const char* instance);
binder_status_t AServiceManager_addService(AIBinder* binder, const char* instance);
// `binder_process.h` is platform-only too; these are the thread-pool
// entry points a service needs to answer anything.
void ABinderProcess_startThreadPool(void);
void ABinderProcess_joinThreadPool(void);
}

// Must match `tests/src/bin/typed_error_probe.rs`.
static constexpr const char* kDescriptor = "rsbinder.test.typederr.IProbe";
static constexpr transaction_code_t kThrow = FIRST_CALL_TRANSACTION;
static constexpr const char* kServerMessage = "typed from libbinder";

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}

// --- server role -----------------------------------------------------
//
// libbinder_ndk has already consumed the interface token by the time this
// runs (it checks it against the class descriptor), so the parcel starts
// at the argument.
static binder_status_t on_transact(AIBinder*, transaction_code_t code, const AParcel* in,
                                   AParcel* out) {
    if (code != kThrow) return STATUS_UNKNOWN_TRANSACTION;

    int32_t requested = 0;
    binder_status_t st = AParcel_readInt32(in, &requested);
    if (st != STATUS_OK) return st;

    AStatus* status = AStatus_fromServiceSpecificErrorWithMessage(requested, kServerMessage);
    st = AParcel_writeStatusHeader(out, status);
    AStatus_delete(status);
    // The transaction itself succeeded; the failure is in the header.
    return st;
}

static int run_server(const char* name) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    if (!clazz) {
        fprintf(stderr, "AIBinder_Class_define failed\n");
        return 2;
    }
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

static int run_client(const char* name, int32_t code) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    if (!clazz) {
        printf("RESULT cpp %d ERROR class-define\n", code);
        return 1;
    }
    AIBinder* binder = AServiceManager_checkService(name);
    if (!binder) {
        printf("RESULT cpp %d ERROR not-registered\n", code);
        return 1;
    }
    if (!AIBinder_associateClass(binder, clazz)) {
        printf("RESULT cpp %d ERROR descriptor-mismatch\n", code);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st == STATUS_OK) st = AParcel_writeInt32(in, code);
    if (st != STATUS_OK) {
        printf("RESULT cpp %d ERROR prepare=%d\n", code, st);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* out = nullptr;
    st = AIBinder_transact(binder, kThrow, &in, &out, 0);
    int rc = 1;
    if (st != STATUS_OK) {
        // A transport-level failure, not the service-specific one under
        // test: the reply never carried a header to read.
        printf("RESULT cpp %d ERROR transact=%d\n", code, st);
    } else {
        AStatus* status = nullptr;
        binder_status_t rst = AParcel_readStatusHeader(out, &status);
        if (rst != STATUS_OK) {
            printf("RESULT cpp %d ERROR read-header=%d\n", code, rst);
        } else {
            binder_exception_t ex = AStatus_getExceptionCode(status);
            if (ex != EX_SERVICE_SPECIFIC) {
                printf("RESULT cpp %d ERROR exception=%d\n", code, ex);
            } else {
                const char* msg = AStatus_getMessage(status);
                printf("RESULT cpp %d SERVICE_SPECIFIC %d %s\n", code,
                       AStatus_getServiceSpecificError(status), msg ? msg : "");
                rc = 0;
            }
            AStatus_delete(status);
        }
        if (out) AParcel_delete(out);
    }
    AIBinder_decStrong(binder);
    return rc;
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) {
        return run_server(argv[2]);
    }
    if (argc == 4 && strcmp(argv[1], "client") == 0) {
        return run_client(argv[2], static_cast<int32_t>(strtol(argv[3], nullptr, 10)));
    }
    fprintf(stderr, "usage: %s client <service-name> <code> | server <service-name>\n", argv[0]);
    return 2;
}

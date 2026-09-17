// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-5 AC-5.4 STAGE3: `android.os.ICancellationSignal` between
// rsbinder and the real AOSP binder stack, both directions.
//
// The transport is a binder object that travels inside a reply, so what
// needs a real peer is whether the other stack's object marshalling and
// `oneway` dispatch agree with ours — neither of which a test with
// rsbinder on both ends can show.
//
//   cancel_interop client <service-name>   # cancel an rsbinder service's transport
//   cancel_interop server <service-name>   # hand out a transport for rsbinder to cancel
//
// Shares its wire contract with `tests/src/bin/cancel_probe.rs`:
// descriptor "rsbinder.test.cancel.IProbe", code 1 = START (reply: the
// transport binder), code 2 = STATUS (reply: int 1 when cancelled).
//
// Linked against the device's own libbinder_ndk.so; the platform-only
// entry points are declared by hand, as in the other launchers here.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <unistd.h>

#include <atomic>
#include <cstdio>
#include <cstring>

extern "C" {
AIBinder* AServiceManager_checkService(const char* instance);
binder_status_t AServiceManager_addService(AIBinder* binder, const char* instance);
void ABinderProcess_startThreadPool(void);
void ABinderProcess_joinThreadPool(void);
}

static constexpr const char* kProbeDescriptor = "rsbinder.test.cancel.IProbe";
// AOSP's own descriptor: this is the interface rsbinder vendored, so the
// token rsbinder writes when it cancels must match it exactly.
static constexpr const char* kSignalDescriptor = "android.os.ICancellationSignal";
static constexpr transaction_code_t kStart = FIRST_CALL_TRANSACTION;
static constexpr transaction_code_t kStatus = FIRST_CALL_TRANSACTION + 1;
static constexpr transaction_code_t kCancel = FIRST_CALL_TRANSACTION;

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}
static binder_status_t on_transact_none(AIBinder*, transaction_code_t, const AParcel*, AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

// --- server role: hand out a transport, report what it saw -----------

static std::atomic<bool> g_canceled{false};

static binder_status_t on_signal_transact(AIBinder*, transaction_code_t code, const AParcel*,
                                          AParcel*) {
    if (code != kCancel) return STATUS_UNKNOWN_TRANSACTION;
    g_canceled.store(true);
    fprintf(stderr, "[cancel_interop] cancel() received\n");
    return STATUS_OK;
}

static binder_status_t on_probe_transact(AIBinder*, transaction_code_t code, const AParcel*,
                                         AParcel* out) {
    static AIBinder_Class* signal_clazz =
        AIBinder_Class_define(kSignalDescriptor, on_create, on_destroy, on_signal_transact);
    switch (code) {
        case kStart: {
            g_canceled.store(false);
            // A fresh transport per job, as the rsbinder side does.
            AIBinder* transport = AIBinder_new(signal_clazz, nullptr);
            binder_status_t st = AParcel_writeStrongBinder(out, transport);
            AIBinder_decStrong(transport);
            return st;
        }
        case kStatus:
            return AParcel_writeInt32(out, g_canceled.load() ? 1 : 0);
        default:
            return STATUS_UNKNOWN_TRANSACTION;
    }
}

static int run_server(const char* name) {
    AIBinder_Class* clazz =
        AIBinder_Class_define(kProbeDescriptor, on_create, on_destroy, on_probe_transact);
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

// --- client role: cancel an rsbinder service's transport -------------

// Returns -1 on failure, else 0/1.
static int read_status(AIBinder* probe) {
    AParcel* in = nullptr;
    if (AIBinder_prepareTransaction(probe, &in) != STATUS_OK) return -1;
    AParcel* out = nullptr;
    if (AIBinder_transact(probe, kStatus, &in, &out, 0) != STATUS_OK) return -1;
    int32_t value = 0;
    binder_status_t st = AParcel_readInt32(out, &value);
    AParcel_delete(out);
    return st == STATUS_OK ? (value != 0 ? 1 : 0) : -1;
}

static int run_client(const char* name) {
    AIBinder_Class* probe_clazz =
        AIBinder_Class_define(kProbeDescriptor, on_create, on_destroy, on_transact_none);
    AIBinder_Class* signal_clazz =
        AIBinder_Class_define(kSignalDescriptor, on_create, on_destroy, on_transact_none);

    AIBinder* probe = AServiceManager_checkService(name);
    if (!probe) {
        printf("RESULT cpp ERROR not-registered\n");
        return 1;
    }
    if (!AIBinder_associateClass(probe, probe_clazz)) {
        printf("RESULT cpp ERROR descriptor-mismatch\n");
        AIBinder_decStrong(probe);
        return 1;
    }

    AParcel* in = nullptr;
    if (AIBinder_prepareTransaction(probe, &in) != STATUS_OK) {
        printf("RESULT cpp ERROR prepare\n");
        AIBinder_decStrong(probe);
        return 1;
    }
    AParcel* out = nullptr;
    binder_status_t st = AIBinder_transact(probe, kStart, &in, &out, 0);
    if (st != STATUS_OK) {
        printf("RESULT cpp ERROR start=%d\n", st);
        AIBinder_decStrong(probe);
        return 1;
    }
    AIBinder* transport = nullptr;
    st = AParcel_readStrongBinder(out, &transport);
    AParcel_delete(out);
    if (st != STATUS_OK || !transport) {
        printf("RESULT cpp ERROR read-transport=%d\n", st);
        AIBinder_decStrong(probe);
        return 1;
    }
    // The descriptor check: rsbinder stamped this binder as
    // `android.os.ICancellationSignal`, and libbinder verifies it by
    // asking the object itself.
    if (!AIBinder_associateClass(transport, signal_clazz)) {
        printf("RESULT cpp ERROR transport-descriptor\n");
        AIBinder_decStrong(transport);
        AIBinder_decStrong(probe);
        return 1;
    }

    int before = read_status(probe);

    AParcel* cin = nullptr;
    st = AIBinder_prepareTransaction(transport, &cin);
    if (st == STATUS_OK) {
        AParcel* cout = nullptr;
        st = AIBinder_transact(transport, kCancel, &cin, &cout, FLAG_ONEWAY);
        if (cout) AParcel_delete(cout);
    }
    if (st != STATUS_OK) {
        printf("RESULT cpp ERROR cancel=%d\n", st);
        AIBinder_decStrong(transport);
        AIBinder_decStrong(probe);
        return 1;
    }

    // Poll: a oneway to the transport's node is not ordered against a
    // twoway to the service's node, so a single read races delivery.
    int after = 0;
    for (int i = 0; i < 50 && after != 1; i++) {
        after = read_status(probe);
        if (after != 1) usleep(20 * 1000);
    }

    printf("RESULT cpp %s %s\n", before == 1 ? "true" : "false", after == 1 ? "true" : "false");
    AIBinder_decStrong(transport);
    AIBinder_decStrong(probe);
    return (before == 0 && after == 1) ? 0 : 1;
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) return run_server(argv[2]);
    if (argc == 3 && strcmp(argv[1], "client") == 0) return run_client(argv[2]);
    fprintf(stderr, "usage: %s client <service-name> | server <service-name>\n", argv[0]);
    return 2;
}

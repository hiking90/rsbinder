// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-9 AC-9.3 STAGE3: the calling work source between rsbinder and the
// real AOSP binder stack.
//
// The work source rides in the request header AOSP `Parcel::writeInterfaceToken`
// writes and `Parcel::enforceInterface` reads. rsbinder writes and reads the
// same field, so a test with rsbinder on both ends would pass even if both
// sides misplaced it. This program is the libbinder end; the rsbinder end is
// `tests/src/bin/work_source_probe.rs`, whose protocol it speaks.
//
//   work_source_interop server  <service-name>
//   work_source_interop get     <service-name> <uid|unset>
//   work_source_interop forward <service-name> <uid|unset> <set-uid|none>
//
// `server` answers GET with the work source and propagate flag libbinder
// installed from the header. `get` sets the work source on its own thread
// (unless `unset`) and calls GET on an rsbinder service. `forward` does the
// same with FORWARD: the rsbinder service calls GET on the next service,
// after setting <set-uid> in its handler unless `none`. Output:
//
//   SERVING <name>
//   RESULT cpp get <uid|unset> SEEN <uid> <propagate>
//   RESULT cpp forward <uid|unset> <set-uid|none> HERE <uid> DOWN <uid>
//   RESULT cpp ... ERROR <detail>
//
// The NDK has no work-source API, so the libbinder calls are resolved from
// the device's `libbinder.so` with `dlsym`: `IPCThreadState::self()` and the
// non-virtual members, called with `this` as the first argument (Itanium
// C++ ABI). `libbinder.so` is already loaded as a dependency of
// `libbinder_ndk.so`, so the same `IPCThreadState` serves both.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <dlfcn.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>

extern "C" {
AIBinder* AServiceManager_checkService(const char* instance);
binder_status_t AServiceManager_addService(AIBinder* binder, const char* instance);
void ABinderProcess_startThreadPool(void);
void ABinderProcess_joinThreadPool(void);
}

// Must match `tests/src/bin/work_source_probe.rs`.
static constexpr const char* kDescriptor = "rsbinder.test.worksource.IProbe";
static constexpr transaction_code_t kGet = FIRST_CALL_TRANSACTION;
static constexpr transaction_code_t kForward = FIRST_CALL_TRANSACTION + 1;
static constexpr int32_t kNoSet = -2;

// --- libbinder work-source calls ---------------------------------------

using SelfFn = void* (*)();
using SetFn = int64_t (*)(void*, uint32_t);
using GetFn = uint32_t (*)(const void*);
using ShouldFn = bool (*)(const void*);

static SelfFn g_self;
static SetFn g_set;
static GetFn g_get;
static ShouldFn g_should;

static const char* resolve() {
    void* lib = dlopen("libbinder.so", RTLD_NOW | RTLD_NOLOAD);
    if (!lib) lib = dlopen("libbinder.so", RTLD_NOW);
    if (!lib) return dlerror();
    g_self = reinterpret_cast<SelfFn>(dlsym(lib, "_ZN7android14IPCThreadState4selfEv"));
    g_set = reinterpret_cast<SetFn>(
            dlsym(lib, "_ZN7android14IPCThreadState23setCallingWorkSourceUidEj"));
    g_get = reinterpret_cast<GetFn>(
            dlsym(lib, "_ZNK7android14IPCThreadState23getCallingWorkSourceUidEv"));
    g_should = reinterpret_cast<ShouldFn>(
            dlsym(lib, "_ZNK7android14IPCThreadState25shouldPropagateWorkSourceEv"));
    if (!g_self || !g_set || !g_get || !g_should) return "dlsym: symbol missing";
    return nullptr;
}

// --- server role --------------------------------------------------------

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}

// libbinder_ndk has checked the interface token, and with it installed the
// header's work source, before this runs. Reply as the rsbinder probe's GET
// does: `[uid][propagate]`, the uid signed so that unset reads as -1.
static binder_status_t on_transact(AIBinder*, transaction_code_t code, const AParcel*,
                                   AParcel* out) {
    if (code != kGet) return STATUS_UNKNOWN_TRANSACTION;
    void* self = g_self();
    binder_status_t st = AParcel_writeInt32(out, static_cast<int32_t>(g_get(self)));
    if (st == STATUS_OK) st = AParcel_writeInt32(out, g_should(self) ? 1 : 0);
    return st;
}

static AIBinder_Class* probe_class() {
    return AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
}

static int run_server(const char* name) {
    AIBinder_Class* clazz = probe_class();
    AIBinder* binder = clazz ? AIBinder_new(clazz, nullptr) : nullptr;
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

// --- client roles -------------------------------------------------------

// `set` is the FORWARD argument, or nullptr for GET.
static int run_client(const char* name, const char* uid, const char* set) {
    char label[64];
    if (set) {
        snprintf(label, sizeof label, "forward %s %s", uid, set);
    } else {
        snprintf(label, sizeof label, "get %s", uid);
    }

    // Set on this thread, outside any transaction: the value the next
    // outgoing header carries.
    if (strcmp(uid, "unset") != 0) {
        g_set(g_self(), static_cast<uint32_t>(strtoul(uid, nullptr, 10)));
    }

    AIBinder_Class* clazz = probe_class();
    AIBinder* binder = AServiceManager_checkService(name);
    if (!binder) {
        printf("RESULT cpp %s ERROR not-registered\n", label);
        return 1;
    }
    if (!AIBinder_associateClass(binder, clazz)) {
        printf("RESULT cpp %s ERROR descriptor-mismatch\n", label);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st == STATUS_OK && set) {
        int32_t arg = strcmp(set, "none") == 0 ? kNoSet
                                               : static_cast<int32_t>(strtol(set, nullptr, 10));
        st = AParcel_writeInt32(in, arg);
    }
    AParcel* out = nullptr;
    if (st == STATUS_OK) st = AIBinder_transact(binder, set ? kForward : kGet, &in, &out, 0);
    int rc = 1;
    int32_t first = 0, second = 0;
    if (st != STATUS_OK) {
        printf("RESULT cpp %s ERROR transact=%d\n", label, st);
    } else if (AParcel_readInt32(out, &first) != STATUS_OK ||
               AParcel_readInt32(out, &second) != STATUS_OK) {
        printf("RESULT cpp %s ERROR short-reply\n", label);
    } else {
        if (set) {
            printf("RESULT cpp %s HERE %d DOWN %d\n", label, first, second);
        } else {
            printf("RESULT cpp %s SEEN %d %d\n", label, first, second);
        }
        rc = 0;
    }
    if (out) AParcel_delete(out);
    AIBinder_decStrong(binder);
    return rc;
}

int main(int argc, char** argv) {
    if (const char* err = resolve()) {
        // The question AC-9.3 left open: whether an executable under
        // /data/local/tmp may load the platform-private libbinder.so.
        printf("RESULT cpp ERROR dlopen %s\n", err);
        return 4;
    }
    if (argc == 3 && strcmp(argv[1], "server") == 0) return run_server(argv[2]);
    if (argc == 4 && strcmp(argv[1], "get") == 0) return run_client(argv[2], argv[3], nullptr);
    if (argc == 5 && strcmp(argv[1], "forward") == 0) {
        return run_client(argv[2], argv[3], argv[4]);
    }
    fprintf(stderr,
            "usage: %s server <name> | get <name> <uid|unset> | "
            "forward <name> <uid|unset> <set-uid|none>\n",
            argv[0]);
    return 2;
}

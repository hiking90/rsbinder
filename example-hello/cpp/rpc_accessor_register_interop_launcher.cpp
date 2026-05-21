// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Subplan 2-14 D.9 STAGE3 — real-libbinder *client* side of the
// IAccessor **register**-side interop. The role-inverse of 2-13 D.8:
// there libbinder served the IAccessor + RPC root and rsbinder
// consumed; here rsbinder serves both and libbinder consumes.
//
// Pairs with `example-hello/src/bin/rpc_accessor_register_interop_server`
// (cross-compiled rsbinder server binary). The companion bash script
// `run_stage3_register.sh` automates build + push + run.
//
// Wire layers verified vs the genuine peer:
//   - kernel-binder `IAccessor` AIDL (`addConnection`) on the rsbinder
//     side, decoded by the NDK `AParcel_readStatusHeader` +
//     `AParcel_readParcelFileDescriptor` (byte-correctness of the
//     reply Parcel marshalled by rsbinder's generated `BnAccessor::
//     on_transact`);
//   - preconnected fd → real-libbinder `ARpcSession_setupPreconnected
//     Client` → 2-8 v2 handshake driven into an rsbinder `RpcServer
//     ::set_android13plus(2)`;
//   - data-bearing RPC transact (TX_ECHO round-trip + TX_GIVE_MARKER
//     byte-correct vs a fixed rsbinder-side marker) — proves rsbinder's
//     RPC reply encoding (`writeStatus + writeString16` on the v2
//     wire) is what real libbinder's BpBinder decoder expects.
//
// Build (host):
//   $ANDROID_NDK_HOME/.../bin/aarch64-linux-android36-clang++ \
//       --target=aarch64-linux-android36 -O2 \
//       -L /tmp -lbinder_ndk -lbinder_rpc_unstable -llog \
//       rpc_accessor_register_interop_launcher.cpp \
//       -o rpc_accessor_register_interop_launcher
// (the `/tmp/libbinder_*.so` are pulled from the device since the
// NDK r29 sysroot ships only the `libbinder_ndk.so` stub.)

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <atomic>
#include <string>

// Platform APIs the NDK r29 sysroot doesn't expose
// (`include_platform/`-only). Declared `extern "C"` with the AOSP
// signatures (cross-checked against
// `frameworks/native/libs/binder/ndk/include_platform/android/
// binder_manager.h` and `binder_rpc.h` /
// `include_rpc_unstable/binder_rpc_unstable.hpp`, android-16.0.0_r4).
// Linked at runtime against the device's libbinder_ndk.so +
// libbinder_rpc_unstable.so.
extern "C" {

// --- libbinder_ndk.so ---
AIBinder* AServiceManager_waitForService(const char* instance);

// Kernel-binder thread pool — needed because we *receive* binder
// transactions (death callbacks etc.) even as a pure client; without
// this, libbinder logs "Thread Pool max thread count is 0..." and
// silently drops link-to-death plumbing.
void ABinderProcess_setThreadPoolMaxThreadCount(uint32_t numThreads);
void ABinderProcess_startThreadPool();

// --- libbinder_rpc_unstable.so ---
struct ARpcSession;
ARpcSession* ARpcSession_new();
void ARpcSession_setMaxIncomingThreads(ARpcSession* session, size_t threads);
void ARpcSession_setMaxOutgoingConnections(ARpcSession* session, size_t connections);
AIBinder* ARpcSession_setupPreconnectedClient(ARpcSession* session,
                                              int (*requestFd)(void* param),
                                              void* param,
                                              void (*paramDeleteFd)(void* param));
void ARpcSession_free(ARpcSession* session);
}

namespace {

constexpr const char* kAccessorDescriptor = "android.os.IAccessor";
// IAccessor AIDL — methods declared in lexicographic order (`addConnection`,
// `getInstanceName`) ⇒ FIRST_CALL_TRANSACTION + {0, 1}. Confirmed
// against the rsbinder generated stub
// (`target/.../out/accessor_16.rs`).
constexpr transaction_code_t TX_ACCESSOR_ADD_CONNECTION = FIRST_CALL_TRANSACTION + 0;

// Must match the rsbinder server's `ROOT_DESC`.
constexpr const char* kRootDescriptor = "rsbinder.test.accessor.IInterop";
// Must match the rsbinder server's TX_* constants.
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_GIVE_MARKER = FIRST_CALL_TRANSACTION + 1;

// Hard-coded marker the rsbinder server hands back for TX_GIVE_MARKER.
// A byte-correct round trip proves the reply Parcel body matches
// (Status header + writeString16, no fd marshalling at this hop).
constexpr const char* kExpectedMarker = "stage3-from-rsbinder";

// Trivial AIBinder_Class callbacks — neither IAccessor nor IInterop is
// ever exposed *as a service* from this process, but the NDK requires
// a class to be associated with any AIBinder before `prepareTransaction`
// will accept it.
void* null_on_create(void* args) { return args; }
void null_on_destroy(void* /*userData*/) {}
binder_status_t null_on_transact(AIBinder* /*binder*/, transaction_code_t /*code*/,
                                 const AParcel* /*in*/, AParcel* /*out*/) {
    return STATUS_UNKNOWN_TRANSACTION;
}

// Adapter for `ARpcSession_setupPreconnectedClient`'s `requestFd`
// callback: hand back the one preconnected fd on the first call, -1
// thereafter. AOSP's single-connection session model only calls this
// once, but defensive against an internal retry surfacing.
struct PreconnectedFd {
    int fd;
    std::atomic<bool> consumed{false};
};

int request_preconnected_fd(void* param) {
    auto* p = static_cast<PreconnectedFd*>(param);
    bool already = p->consumed.exchange(true, std::memory_order_acq_rel);
    if (already) return -1;
    // Hand off ownership: the AOSP `setupPreconnectedClient` adopts the
    // fd and closes it on session shutdown. After this returns, our
    // local copy must not also `close(p->fd)`.
    int fd = p->fd;
    p->fd = -1;
    return fd;
}

void delete_preconnected_fd(void* /*param*/) {
    // Storage is stack-allocated in main(); nothing to free.
}

// AParcel_readString allocator → std::string sink. Identical shape to
// the 2-13 D.8 launcher's `root_on_transact` allocator — kept verbatim
// so a future divergence between the two harnesses is the bug, not
// the harness shape.
bool read_string_into(void* opaque, int32_t length, char** outBuf) {
    auto* dst = static_cast<std::string*>(opaque);
    if (length < 0) {
        *outBuf = nullptr;
        return true;
    }
    dst->resize(length);
    *outBuf = dst->data();
    return true;
}

// AParcel ownership helpers — RAII so an early `return` doesn't leak.
struct InParcel {
    AParcel* p = nullptr;
    ~InParcel() {
        if (p) AParcel_delete(p);
    }
};
struct OutParcel {
    AParcel* p = nullptr;
    ~OutParcel() {
        if (p) AParcel_delete(p);
    }
};
struct AStatusOwned {
    AStatus* s = nullptr;
    ~AStatusOwned() {
        if (s) AStatus_delete(s);
    }
};

} // namespace

int main(int argc, char** argv) {
    const char* instance = (argc > 1) ? argv[1] : "rsbinder.test.acc.reg";
    fprintf(stderr, "[cpp-client] STAGE3 register-side instance=%s\n", instance);

    // 0) Kernel-binder thread pool — same reason the rsbinder server
    //    spins one up: an AIBinder we *use* needs a thread pool ready
    //    so libbinder's internal machinery (link-to-death, async
    //    bookkeeping) can run.
    ABinderProcess_setThreadPoolMaxThreadCount(2);
    ABinderProcess_startThreadPool();

    // 1) Block on the IAccessor binder showing up in the kernel service
    //    manager. `AServiceManager_waitForService` keeps retrying with
    //    a server-side wait, exiting only when the service appears
    //    (matches the 2-13 D.8 launcher's `addService → wait` ordering
    //    on the rsbinder client side — symmetric here).
    AIBinder* accessor_raw = AServiceManager_waitForService(instance);
    if (!accessor_raw) {
        fprintf(stderr, "[cpp-client] AServiceManager_waitForService(%s) returned null\n",
                instance);
        return 1;
    }
    // M16 fix (review 2026-05-21): RAII for the `accessor` binder.
    // Previously every early return after this point had to remember
    // an explicit `AIBinder_decStrong(accessor)`; the unique_ptr makes
    // cleanup automatic and symmetric with the normal-exit path.
    auto accessor_owner = std::unique_ptr<AIBinder, decltype(&AIBinder_decStrong)>(
            accessor_raw, AIBinder_decStrong);
    AIBinder* accessor = accessor_owner.get();
    fprintf(stderr, "[cpp-client] obtained IAccessor binder\n");

    // 2) Associate the AIBinder with a class carrying the IAccessor
    //    descriptor. `AIBinder_prepareTransaction` writes the
    //    interface token header from this class's descriptor, so this
    //    is what makes the next transact's interface-token byte-
    //    correct against `android.os.IAccessor` (cross-checked against
    //    the rsbinder generated stub).
    AIBinder_Class* accessor_clazz = AIBinder_Class_define(
            kAccessorDescriptor, null_on_create, null_on_destroy, null_on_transact);
    if (!accessor_clazz) {
        fprintf(stderr, "[cpp-client] AIBinder_Class_define(IAccessor) failed\n");
        return 2;
    }
    if (!AIBinder_associateClass(accessor, accessor_clazz)) {
        fprintf(stderr, "[cpp-client] AIBinder_associateClass(IAccessor) failed\n");
        return 3;
    }

    // 3) Call addConnection() over kernel binder. Reply marshalling
    //    (per AIDL): `Status::Ok` header (12B on android-13+) +
    //    non-nullable `ParcelFileDescriptor` (the kernel FLAT_BINDER_OBJECT
    //    for an fd). NDK helpers handle both.
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(accessor, &in.p);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] prepareTransaction(addConnection): %d\n", rc);
        return 4;
    }
    OutParcel out;
    rc = AIBinder_transact(accessor, TX_ACCESSOR_ADD_CONNECTION, &in.p, &out.p, 0);
    // `transact` consumed `in.p`. Clear our RAII handle.
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(addConnection): %d (%s)\n", rc, strerror(errno));
        return 5;
    }
    AStatusOwned aidl_status;
    rc = AParcel_readStatusHeader(out.p, &aidl_status.s);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] readStatusHeader(addConnection): %d\n", rc);
        return 6;
    }
    if (!AStatus_isOk(aidl_status.s)) {
        fprintf(stderr, "[cpp-client] addConnection returned non-OK Status: ex=%d ss=%d msg=%s\n",
                AStatus_getExceptionCode(aidl_status.s),
                AStatus_getServiceSpecificError(aidl_status.s),
                AStatus_getMessage(aidl_status.s));
        return 7;
    }
    int pfd = -1;
    rc = AParcel_readParcelFileDescriptor(out.p, &pfd);
    if (rc != STATUS_OK || pfd < 0) {
        fprintf(stderr, "[cpp-client] readParcelFileDescriptor: rc=%d fd=%d\n", rc, pfd);
        return 8;
    }
    fprintf(stderr, "[cpp-client] addConnection → fd=%d\n", pfd);

    // 4) Wrap the fd in a preconnected ARpcSession. The session is the
    //    real-libbinder consumer of the v2 handshake initiated by
    //    rsbinder's `RpcServer::set_android13plus(2)`.
    ARpcSession* session_raw = ARpcSession_new();
    if (!session_raw) {
        fprintf(stderr, "[cpp-client] ARpcSession_new failed\n");
        close(pfd);
        return 9;
    }
    // M16 RAII (review 2026-05-21).
    auto session_owner = std::unique_ptr<ARpcSession, decltype(&ARpcSession_free)>(
            session_raw, ARpcSession_free);
    ARpcSession* session = session_owner.get();
    // Single-connection session — matches rsbinder server's
    // `set_max_threads(1)` (one outgoing connection per session).
    ARpcSession_setMaxIncomingThreads(session, 0);
    ARpcSession_setMaxOutgoingConnections(session, 1);

    PreconnectedFd pfd_state{.fd = pfd};
    AIBinder* root_raw = ARpcSession_setupPreconnectedClient(
            session, request_preconnected_fd, &pfd_state, delete_preconnected_fd);
    if (!root_raw) {
        fprintf(stderr, "[cpp-client] setupPreconnectedClient returned null (handshake failed)\n");
        if (pfd_state.fd >= 0) close(pfd_state.fd);
        return 10;
    }
    auto root_owner = std::unique_ptr<AIBinder, decltype(&AIBinder_decStrong)>(
            root_raw, AIBinder_decStrong);
    AIBinder* root = root_owner.get();
    fprintf(stderr, "[cpp-client] RPC root acquired via preconnected fd\n");

    // 5) Associate the root with the IInterop class so we can transact
    //    with byte-correct interface-token headers.
    AIBinder_Class* interop_clazz = AIBinder_Class_define(
            kRootDescriptor, null_on_create, null_on_destroy, null_on_transact);
    if (!interop_clazz) {
        fprintf(stderr, "[cpp-client] AIBinder_Class_define(IInterop) failed\n");
        return 11;
    }
    if (!AIBinder_associateClass(root, interop_clazz)) {
        fprintf(stderr, "[cpp-client] AIBinder_associateClass(IInterop) failed\n");
        return 12;
    }

    // 6a) TX_ECHO("hello-stage3-reg") — round-trip a String. Exercises
    //     the Parcel body bytes (libbinder writes, rsbinder reads,
    //     rsbinder writes reply, libbinder reads) ⇒ byte-faithfulness
    //     of the v2 wire including the AIDL Status header on the
    //     reply.
    const char* echo_arg = "hello-stage3-reg";
    {
        InParcel ein;
        rc = AIBinder_prepareTransaction(root, &ein.p);
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] prepareTransaction(TX_ECHO): %d\n", rc);
            return 13;
        }
        rc = AParcel_writeString(ein.p, echo_arg, (int32_t)strlen(echo_arg));
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] writeString(TX_ECHO arg): %d\n", rc);
            return 14;
        }
        OutParcel eout;
        rc = AIBinder_transact(root, TX_ECHO, &ein.p, &eout.p, 0);
        ein.p = nullptr;
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] transact(TX_ECHO): %d\n", rc);
            return 15;
        }
        AStatusOwned st;
        rc = AParcel_readStatusHeader(eout.p, &st.s);
        if (rc != STATUS_OK || !AStatus_isOk(st.s)) {
            fprintf(stderr, "[cpp-client] TX_ECHO non-OK Status\n");
            return 16;
        }
        std::string echoed;
        rc = AParcel_readString(eout.p, &echoed, read_string_into);
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] readString(TX_ECHO reply): %d\n", rc);
            return 17;
        }
        // Drop the NDK allocator's trailing NUL (matches 2-13 D.8
        // launcher's TX_ECHO handler convention).
        if (!echoed.empty() && echoed.back() == '\0') echoed.pop_back();
        fprintf(stderr, "[cpp-client] TX_ECHO reply=%.*s\n", (int)echoed.size(), echoed.data());
        if (echoed != echo_arg) {
            fprintf(stderr, "[cpp-client] TX_ECHO mismatch: got %.*s, want %s\n",
                    (int)echoed.size(), echoed.data(), echo_arg);
            return 18;
        }
    }

    // 6b) TX_GIVE_MARKER — no arg, fixed server-side string. A byte-
    //     correct reply proves the reply Parcel body is right against
    //     a zero-byte request (no client-side write into the body, so
    //     the trip has no "we'd have noticed any garbage" mask).
    {
        InParcel min;
        rc = AIBinder_prepareTransaction(root, &min.p);
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] prepareTransaction(TX_GIVE_MARKER): %d\n", rc);
            return 19;
        }
        OutParcel mout;
        rc = AIBinder_transact(root, TX_GIVE_MARKER, &min.p, &mout.p, 0);
        min.p = nullptr;
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] transact(TX_GIVE_MARKER): %d\n", rc);
            return 20;
        }
        AStatusOwned st;
        rc = AParcel_readStatusHeader(mout.p, &st.s);
        if (rc != STATUS_OK || !AStatus_isOk(st.s)) {
            fprintf(stderr, "[cpp-client] TX_GIVE_MARKER non-OK Status\n");
            return 21;
        }
        std::string marker;
        rc = AParcel_readString(mout.p, &marker, read_string_into);
        if (rc != STATUS_OK) {
            fprintf(stderr, "[cpp-client] readString(TX_GIVE_MARKER reply): %d\n", rc);
            return 22;
        }
        if (!marker.empty() && marker.back() == '\0') marker.pop_back();
        fprintf(stderr, "[cpp-client] TX_GIVE_MARKER reply=%.*s\n",
                (int)marker.size(), marker.data());
        if (marker != kExpectedMarker) {
            fprintf(stderr, "[cpp-client] TX_GIVE_MARKER mismatch: got %.*s, want %s\n",
                    (int)marker.size(), marker.data(), kExpectedMarker);
            return 23;
        }
    }

    printf("STAGE3 PASS — real libbinder client ↔ rsbinder server full transact\n");
    fflush(stdout);
    // M16 RAII: `accessor_owner`/`session_owner`/`root_owner` clean up
    // automatically on return.
    return 0;
}

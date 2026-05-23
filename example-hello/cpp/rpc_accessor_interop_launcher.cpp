// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Subplan 2-13 D.7/D.8 STAGE3 — real-libbinder side of the IAccessor
// bridge interop. Stands up an android-16 libbinder RPC server, wraps
// it in `ABinderRpc_Accessor` (the genuine libbinder IAccessor
// implementation), registers the Accessor binder via
// `AServiceManager_addService`, and blocks. The rsbinder client
// (`rpc_accessor_interop_client`) then:
//
//   1. `hub::get_service("rsbinder.test.acc")` over kernel binder ⇒
//      ServiceWithMetadata arm (no VINTF entry, so servicemanager
//      stays out of the `Service::accessor` arm — the bridge being
//      tested is `accessor_16::resolve_accessor`, which is profile-
//      independent of the arm dispatch);
//   2. calls `resolve_accessor(name, accessor_binder)` directly →
//      `BpAccessor::addConnection()` (real-libbinder wire) → fd adopt
//      → `RpcSession::from_preconnected_fd` → 2-8 v2 handshake →
//      `get_root()` → full data-bearing transact.
//
// Wire layers verified:
//   - kernel-binder `IAccessor` AIDL (addConnection/getInstanceName)
//     against real libbinder_ndk `ABinderRpc_Accessor`;
//   - preconnected fd → rsbinder `from_preconnected_fd` family judge;
//   - android-13+ versioned RPC handshake + Parcel body against the
//     real libbinder RPC server (`ARpcServer_newBoundSocket`).
//
// `Service::accessor` arm of `getService2` requires a VINTF manifest
// `<accessor>` entry (AOSP `ServiceManager.cpp:431-446`) and a /system
// remount that the stock emulator doesn't allow. That path is plan
// 2-14 territory (registration side); STAGE3 here verifies the bridge
// proper.
//
// Build (host):
//   $ANDROID_NDK_HOME/.../bin/aarch64-linux-android36-clang++ \
//       --target=aarch64-linux-android36 -O2 \
//       -L /tmp -lbinder_ndk -lbinder_rpc_unstable -llog \
//       rpc_accessor_interop_launcher.cpp -o rpc_accessor_interop_launcher
// (the `/tmp/libbinder_*.so` are pulled from the device since the
// NDK r29 sysroot ships only the `libbinder_ndk.so` stub.)

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>

// Platform APIs the NDK r29 sysroot doesn't expose
// (`include_platform/`-only). Declared `extern "C"` with the AOSP
// signatures (cross-checked against
// `frameworks/native/libs/binder/ndk/include_platform/android/binder_manager.h`
// and `binder_rpc.h` / `include_rpc_unstable/binder_rpc_unstable.hpp`,
// android-16.0.0_r4). Linked at runtime against the device's
// libbinder_ndk.so + libbinder_rpc_unstable.so.
extern "C" {

// --- libbinder_ndk.so ---
binder_exception_t AServiceManager_addService(AIBinder* binder, const char* instance);

// Kernel-binder thread pool for the launcher process: required so the
// `BBinder` underneath our `ABinderRpc_Accessor` AIBinder can actually
// receive `addConnection()` / `getInstanceName()` calls from clients.
// Without this, libbinder warns
// "Thread Pool max thread count is 0. Cannot cache binder as
//  linkToDeath cannot be implemented" and the kernel queues
// transactions with no thread to dispatch — the client hangs in
// `binder_thread_read`.
void ABinderProcess_setThreadPoolMaxThreadCount(uint32_t numThreads);
void ABinderProcess_startThreadPool();

struct ABinderRpc_Accessor;
struct ABinderRpc_ConnectionInfo;

typedef ABinderRpc_ConnectionInfo* (*ABinderRpc_ConnectionInfoProvider_t)(
        const char* instance, void* data);
typedef void (*ABinderRpc_ConnectionInfoProviderUserData_delete_t)(void* data);

ABinderRpc_Accessor* ABinderRpc_Accessor_new(
        const char* instance,
        ABinderRpc_ConnectionInfoProvider_t provider,
        void* data,
        ABinderRpc_ConnectionInfoProviderUserData_delete_t onDelete);

void ABinderRpc_Accessor_delete(ABinderRpc_Accessor* accessor);
AIBinder* ABinderRpc_Accessor_asBinder(ABinderRpc_Accessor* accessor);

ABinderRpc_ConnectionInfo* ABinderRpc_ConnectionInfo_new(const sockaddr* addr, socklen_t len);
void ABinderRpc_ConnectionInfo_delete(ABinderRpc_ConnectionInfo* info);

// --- libbinder_rpc_unstable.so ---
struct ARpcServer;
ARpcServer* ARpcServer_newBoundSocket(AIBinder* service, int socketFd);
void ARpcServer_setMaxThreads(ARpcServer* server, size_t threads);
void ARpcServer_start(ARpcServer* server);
void ARpcServer_join(ARpcServer* server);
}

namespace {

constexpr const char* kRootDescriptor = "rsbinder.test.accessor.IInterop";
// Transactions: must match the rsbinder client's TX_* constants.
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION;        // echo(String) -> String
constexpr transaction_code_t TX_GIVE_MARKER = FIRST_CALL_TRANSACTION + 1; // () -> String

// `data` field of the AIBinder root service. The class onCreate
// returns this and onTransact uses it; we keep it empty (a pure
// stateless echo).
struct RootState {
    std::atomic<uint32_t> call_count{0};
};

void* root_on_create(void* args) {
    return args; // pass-through; we never `new` in onCreate so destroy is also trivial
}
void root_on_destroy(void* /*userData*/) {
    // RootState is owned by main(); nothing to free here.
}

binder_status_t root_on_transact(AIBinder* binder, transaction_code_t code,
                                  const AParcel* in, AParcel* out) {
    RootState* st = static_cast<RootState*>(AIBinder_getUserData(binder));
    if (st) st->call_count.fetch_add(1, std::memory_order_relaxed);

    switch (code) {
        case TX_ECHO: {
            // AParcel_readString uses an allocator callback. Use a
            // std::string sink — the simplest cross-compat shape.
            std::string s;
            binder_status_t rc = AParcel_readString(
                    in, &s,
                    [](void* opaque, int32_t length, char** outBuf) -> bool {
                        auto* dst = static_cast<std::string*>(opaque);
                        if (length < 0) {
                            *outBuf = nullptr;
                            return true;
                        }
                        dst->resize(length);
                        *outBuf = dst->data();
                        return true;
                    });
            if (rc != STATUS_OK) return rc;
            // Drop the trailing NUL the NDK allocator wrote.
            if (!s.empty() && s.back() == '\0') s.pop_back();
            fprintf(stderr, "[cpp-server] TX_ECHO arg=%.*s\n", (int)s.size(), s.data());
            // AIDL convention for `Status::Ok` then payload.
            binder_status_t st0 = AParcel_writeInt32(out, 0);
            if (st0 != STATUS_OK) return st0;
            return AParcel_writeString(out, s.data(), (int32_t)s.size());
        }
        case TX_GIVE_MARKER: {
            const char marker[] = "stage3-from-real-libbinder";
            binder_status_t st0 = AParcel_writeInt32(out, 0);
            if (st0 != STATUS_OK) return st0;
            return AParcel_writeString(out, marker, sizeof(marker) - 1);
        }
        default:
            return STATUS_UNKNOWN_TRANSACTION;
    }
}

// Connection-info provider for ABinderRpc_Accessor. Hands back the
// sockaddr_un for the bound RPC socket on every call.
struct ConnInfoArg {
    std::string sock_path;
};

ABinderRpc_ConnectionInfo* conn_info_provider(const char* instance, void* data) {
    auto* arg = static_cast<ConnInfoArg*>(data);
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (arg->sock_path.size() >= sizeof(addr.sun_path)) {
        fprintf(stderr, "[cpp-server] socket path too long: %s\n", arg->sock_path.c_str());
        return nullptr;
    }
    strncpy(addr.sun_path, arg->sock_path.c_str(), sizeof(addr.sun_path) - 1);
    // AOSP `ABinderRpc_ConnectionInfo_new` for `AF_UNIX` requires
    // **exactly** `sizeof(sockaddr_un)` (frameworks/native/libs/binder/
    // ndk/binder_rpc.cpp:347) — not the trimmed `offsetof + strlen + 1`
    // length some sockaddr APIs use. Pass the full struct size.
    socklen_t len = (socklen_t)sizeof(addr);
    fprintf(stderr, "[cpp-server] conn_info_provider(instance=%s) -> %s\n",
            instance, arg->sock_path.c_str());
    return ABinderRpc_ConnectionInfo_new(reinterpret_cast<const sockaddr*>(&addr), len);
}

void conn_info_delete(void* data) {
    delete static_cast<ConnInfoArg*>(data);
}

int bind_uds_listen(const std::string& path) {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("[cpp-server] socket");
        return -1;
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (path.size() >= sizeof(addr.sun_path)) {
        fprintf(stderr, "[cpp-server] socket path too long\n");
        close(fd);
        return -1;
    }
    strncpy(addr.sun_path, path.c_str(), sizeof(addr.sun_path) - 1);
    unlink(path.c_str());
    socklen_t len = (socklen_t)(offsetof(sockaddr_un, sun_path) + path.size() + 1);
    if (bind(fd, reinterpret_cast<sockaddr*>(&addr), len) < 0) {
        perror("[cpp-server] bind");
        close(fd);
        return -1;
    }
    if (listen(fd, 5) < 0) {
        perror("[cpp-server] listen");
        close(fd);
        return -1;
    }
    // Permissive ACL on the path so the client (same uid in tests, but
    // be explicit) can connect().
    chmod(path.c_str(), 0666);
    return fd;
}

} // namespace

int main(int argc, char** argv) {
    const char* instance = (argc > 1) ? argv[1] : "rsbinder.test.acc";
    const char* sock_path = (argc > 2) ? argv[2] : "/data/local/tmp/rsacc-rpc.sock";

    fprintf(stderr, "[cpp-server] instance=%s sock=%s\n", instance, sock_path);

    // 0) Spin up the kernel-binder thread pool so the accessor binder
    //    can actually service `addConnection()` and `getInstanceName()`
    //    transactions. Without this, the client hangs forever.
    ABinderProcess_setThreadPoolMaxThreadCount(4);
    ABinderProcess_startThreadPool();

    // 1) Bind the UDS socket.
    int sockfd = bind_uds_listen(sock_path);
    if (sockfd < 0) return 1;

    // 2) Build the root AIBinder.
    AIBinder_Class* clazz = AIBinder_Class_define(
            kRootDescriptor, root_on_create, root_on_destroy, root_on_transact);
    if (!clazz) {
        fprintf(stderr, "[cpp-server] AIBinder_Class_define failed\n");
        return 2;
    }
    auto* root_state = new RootState();
    AIBinder* root_binder = AIBinder_new(clazz, root_state);
    if (!root_binder) {
        fprintf(stderr, "[cpp-server] AIBinder_new failed\n");
        return 3;
    }

    // 3) Start the libbinder RPC server on the bound socket.
    ARpcServer* server = ARpcServer_newBoundSocket(root_binder, sockfd);
    if (!server) {
        fprintf(stderr, "[cpp-server] ARpcServer_newBoundSocket failed\n");
        return 4;
    }
    ARpcServer_setMaxThreads(server, 2);
    // Note: do NOT call `ARpcServer_start` here — that spawns a join
    // thread, and a later `ARpcServer_join` then prints "Already
    // joined" and returns immediately, exiting main(). The serve loop
    // runs on this main thread instead.
    fprintf(stderr, "[cpp-server] RPC server listening on %s\n", sock_path);

    // 4) Create the IAccessor binder backed by libbinder.
    auto* arg = new ConnInfoArg{std::string(sock_path)};
    ABinderRpc_Accessor* accessor =
            ABinderRpc_Accessor_new(instance, conn_info_provider, arg, conn_info_delete);
    if (!accessor) {
        fprintf(stderr, "[cpp-server] ABinderRpc_Accessor_new failed\n");
        return 5;
    }
    AIBinder* accessor_binder = ABinderRpc_Accessor_asBinder(accessor);
    if (!accessor_binder) {
        fprintf(stderr, "[cpp-server] ABinderRpc_Accessor_asBinder returned null\n");
        return 6;
    }

    // 5) Register with the kernel servicemanager. (Not the
    //    `Service::accessor` arm — that needs a VINTF entry; STAGE3
    //    here exercises the bridge with any kernel-binder IAccessor.)
    binder_exception_t st = AServiceManager_addService(accessor_binder, instance);
    if (st != EX_NONE) {
        fprintf(stderr, "[cpp-server] AServiceManager_addService failed: %d\n", st);
        return 7;
    }
    fprintf(stderr, "[cpp-server] addService(%s) OK; READY (joining)\n", instance);
    printf("[cpp-server] READY\n");
    fflush(stdout);

    // 6) Block on the RPC server's serve thread (Ctrl-C / kill ends it).
    ARpcServer_join(server);

    // Cleanup (unreachable in practice — kill stops us).
    AIBinder_decStrong(accessor_binder);
    ABinderRpc_Accessor_delete(accessor);
    AIBinder_decStrong(root_binder);
    delete root_state;
    return 0;
}

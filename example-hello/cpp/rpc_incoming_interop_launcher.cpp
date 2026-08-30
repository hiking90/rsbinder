// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-20 STAGE3 gate (e) — the **reverse** direction of gate (d) in
// `rpc_multiconn_interop_launcher.cpp`: a real-libbinder **RPC server**
// (this launcher) and an rsbinder **client** that opened one incoming
// connection (`RpcUnixClientConfig::incoming_connections(1)`).
//
// The client hands us a callback and asks (`TX_SCHEDULE_CALLBACK`) for it
// to be driven later. We do that from a plain `std::thread` — no handler
// on the stack — which in libbinder means `ExclusiveConnection::find`
// must pick a connection from the session's `mOutgoing` pool. That pool
// holds exactly the connection the rsbinder client attached with the
// INCOMING header bit; without it libbinder fails with WOULD_BLOCK.
// The rsbinder client serves that connection on its own thread and
// answers the twoway echo, then receives a oneway notify.
//
// Wire: android-13+ v2 (whatever the on-device libbinder speaks; the
// rsbinder client asks for max_version=2). Interface token: bare
// `writeString16(descriptor)` (the STAGE3 RPC convention).

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
#include <chrono>
#include <mutex>
#include <string>
#include <thread>

extern "C" {
void ABinderProcess_setThreadPoolMaxThreadCount(uint32_t numThreads);
void ABinderProcess_startThreadPool();

// --- libbinder_rpc_unstable.so ---
struct ARpcServer;
ARpcServer* ARpcServer_newBoundSocket(AIBinder* service, int socketFd);
void ARpcServer_setMaxThreads(ARpcServer* server, size_t threads);
void ARpcServer_join(ARpcServer* server);
}

namespace {

constexpr const char* kRootDescriptor = "rsbinder.test.IIncomingInterop";
constexpr const char* kCallbackDescriptor = "rsbinder.test.IIncomingInteropCallback";

// Must match the rsbinder client's TX_* constants.
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_SCHEDULE_CALLBACK = FIRST_CALL_TRANSACTION + 5;
constexpr transaction_code_t TX_GET_SCHED = FIRST_CALL_TRANSACTION + 6;
constexpr transaction_code_t TX_CALLBACK_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_CALLBACK_NOTIFY = FIRST_CALL_TRANSACTION + 1; // oneway

struct AStatusOwned {
    AStatus* s = nullptr;
    ~AStatusOwned() {
        if (s) AStatus_delete(s);
    }
};
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

// Outcome of the scheduled callback, read back via TX_GET_SCHED.
std::mutex g_sched_mu;
std::string g_sched = "pending";

void set_sched(std::string s) {
    std::lock_guard<std::mutex> l(g_sched_mu);
    g_sched = std::move(s);
}

AIBinder_Class* g_cb_clazz = nullptr;

binder_status_t cb_stub_on_transact(AIBinder*, transaction_code_t, const AParcel*, AParcel*) {
    // Never called: the class only gives the *proxy* its descriptor.
    return STATUS_UNKNOWN_TRANSACTION;
}
void* on_create(void* args) { return args; }
void on_destroy(void* /*userData*/) {}

// The out-of-handler driver: runs on a plain thread ~150 ms after the
// request that parked `cb` has been answered.
void drive_callback_later(AIBinder* cb, std::string arg) {
    std::this_thread::sleep_for(std::chrono::milliseconds(150));
    std::string outcome;
    do {
        InParcel in;
        binder_status_t rc = AIBinder_prepareTransaction(cb, &in.p);
        if (rc != STATUS_OK) {
            outcome = "err:prepare:" + std::to_string(rc);
            break;
        }
        rc = AParcel_writeString(in.p, arg.c_str(), (int32_t)arg.size());
        if (rc != STATUS_OK) {
            outcome = "err:writeString:" + std::to_string(rc);
            break;
        }
        OutParcel out;
        rc = AIBinder_transact(cb, TX_CALLBACK_ECHO, &in.p, &out.p, 0);
        in.p = nullptr;
        if (rc != STATUS_OK) {
            outcome = "err:transact:" + std::to_string(rc);
            break;
        }
        AStatusOwned st;
        rc = AParcel_readStatusHeader(out.p, &st.s);
        if (rc != STATUS_OK || !AStatus_isOk(st.s)) {
            outcome = "err:status";
            break;
        }
        std::string echoed;
        rc = AParcel_readString(out.p, &echoed, read_string_into);
        if (rc != STATUS_OK) {
            outcome = "err:readString:" + std::to_string(rc);
            break;
        }
        if (!echoed.empty() && echoed.back() == '\0') echoed.pop_back();

        InParcel in2;
        rc = AIBinder_prepareTransaction(cb, &in2.p);
        if (rc != STATUS_OK) {
            outcome = "err:prepare2:" + std::to_string(rc);
            break;
        }
        OutParcel out2;
        rc = AIBinder_transact(cb, TX_CALLBACK_NOTIFY, &in2.p, &out2.p, FLAG_ONEWAY);
        in2.p = nullptr;
        if (rc != STATUS_OK) {
            outcome = "err:oneway:" + std::to_string(rc);
            break;
        }
        outcome = "ok:" + echoed;
    } while (false);
    fprintf(stderr, "[cpp-server] scheduled callback outside a handler: %s\n", outcome.c_str());
    set_sched(outcome);
    AIBinder_decStrong(cb);
}

binder_status_t root_on_transact(AIBinder* /*binder*/, transaction_code_t code,
                                 const AParcel* in, AParcel* out) {
    switch (code) {
        case TX_ECHO: {
            std::string s;
            binder_status_t rc = AParcel_readString(in, &s, read_string_into);
            if (rc != STATUS_OK) return rc;
            if (!s.empty() && s.back() == '\0') s.pop_back();
            fprintf(stderr, "[cpp-server] TX_ECHO arg=%.*s\n", (int)s.size(), s.data());
            binder_status_t st0 = AParcel_writeInt32(out, 0);
            if (st0 != STATUS_OK) return st0;
            return AParcel_writeString(out, s.data(), (int32_t)s.size());
        }
        case TX_SCHEDULE_CALLBACK: {
            AIBinder* cb = nullptr;
            binder_status_t rc = AParcel_readStrongBinder(in, &cb);
            if (rc != STATUS_OK || !cb) {
                fprintf(stderr, "[cpp-server] readStrongBinder: %d\n", rc);
                return rc != STATUS_OK ? rc : STATUS_BAD_VALUE;
            }
            if (!AIBinder_associateClass(cb, g_cb_clazz)) {
                fprintf(stderr, "[cpp-server] associateClass(cb) failed\n");
                AIBinder_decStrong(cb);
                return STATUS_BAD_TYPE;
            }
            std::string s;
            rc = AParcel_readString(in, &s, read_string_into);
            if (rc != STATUS_OK) {
                AIBinder_decStrong(cb);
                return rc;
            }
            if (!s.empty() && s.back() == '\0') s.pop_back();
            set_sched("pending");
            // `cb` (already +1 strong from readStrongBinder) is released
            // by the thread when done.
            std::thread(drive_callback_later, cb, s).detach();
            fprintf(stderr, "[cpp-server] TX_SCHEDULE_CALLBACK parked; driving in 150 ms\n");
            return AParcel_writeInt32(out, 0);
        }
        case TX_GET_SCHED: {
            std::string s;
            {
                std::lock_guard<std::mutex> l(g_sched_mu);
                s = g_sched;
            }
            binder_status_t st0 = AParcel_writeInt32(out, 0);
            if (st0 != STATUS_OK) return st0;
            return AParcel_writeString(out, s.data(), (int32_t)s.size());
        }
        default:
            return STATUS_UNKNOWN_TRANSACTION;
    }
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
    chmod(path.c_str(), 0666);
    return fd;
}

} // namespace

int main(int argc, char** argv) {
    const char* sock_path = (argc > 1) ? argv[1] : "/data/local/tmp/rsinc.sock";
    fprintf(stderr, "[cpp-server] plan 2-20 gate (e): libbinder RpcServer on %s\n", sock_path);

    ABinderProcess_setThreadPoolMaxThreadCount(2);
    ABinderProcess_startThreadPool();

    int sockfd = bind_uds_listen(sock_path);
    if (sockfd < 0) return 1;

    g_cb_clazz = AIBinder_Class_define(kCallbackDescriptor, on_create, on_destroy,
                                       cb_stub_on_transact);
    AIBinder_Class* root_clazz =
            AIBinder_Class_define(kRootDescriptor, on_create, on_destroy, root_on_transact);
    if (!g_cb_clazz || !root_clazz) {
        fprintf(stderr, "[cpp-server] AIBinder_Class_define failed\n");
        return 2;
    }
    AIBinder* root_binder = AIBinder_new(root_clazz, nullptr);
    if (!root_binder) {
        fprintf(stderr, "[cpp-server] AIBinder_new failed\n");
        return 3;
    }
    ARpcServer* server = ARpcServer_newBoundSocket(root_binder, sockfd);
    if (!server) {
        fprintf(stderr, "[cpp-server] ARpcServer_newBoundSocket failed\n");
        return 4;
    }
    ARpcServer_setMaxThreads(server, 2);
    fprintf(stderr, "[cpp-server] READY (joining)\n");
    printf("[cpp-server] READY\n");
    fflush(stdout);
    ARpcServer_join(server);
    AIBinder_decStrong(root_binder);
    return 0;
}

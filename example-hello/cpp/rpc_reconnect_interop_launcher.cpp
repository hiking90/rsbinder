// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-10b AC-10b.10 STAGE3 — a real-libbinder RPC server whose only
// object is its root (`RpcServer::setRootObject`, here
// `ARpcServer_newBoundSocket`), the server `rsbinder::Reconnecting` reaches
// with a fragment-less URI. `run_rpc_reconnect_interop.sh` kills it and
// starts another on the same socket; the root's echo answers `<tag>:<arg>`,
// so the rsbinder client can tell which instance it is talking to.
//
// Usage: rpc_reconnect_interop_launcher <socket path> <tag>

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <stddef.h>
#include <stdio.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

#include <string>

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

// example-hello's `hello.IHello` (`aidl/hello/IHello.aidl`): `String echo(in String)`.
constexpr const char* kRootDescriptor = "hello.IHello";
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;

std::string g_tag;

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

void* on_create(void* args) { return args; }
void on_destroy(void* /*userData*/) {}

binder_status_t root_on_transact(AIBinder* /*binder*/, transaction_code_t code,
                                 const AParcel* in, AParcel* out) {
    if (code != TX_ECHO) return STATUS_UNKNOWN_TRANSACTION;
    std::string s;
    binder_status_t rc = AParcel_readString(in, &s, read_string_into);
    if (rc != STATUS_OK) return rc;
    if (!s.empty() && s.back() == '\0') s.pop_back();
    std::string reply = g_tag + ":" + s;
    fprintf(stderr, "[cpp-server] TX_ECHO %s\n", reply.c_str());
    // The generated rsbinder proxy reads a `Status` first: exception 0 = none.
    rc = AParcel_writeInt32(out, 0);
    if (rc != STATUS_OK) return rc;
    return AParcel_writeString(out, reply.data(), (int32_t)reply.size());
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
    if (argc != 3) {
        fprintf(stderr, "usage: %s <socket path> <tag>\n", argv[0]);
        return 2;
    }
    g_tag = argv[2];
    fprintf(stderr, "[cpp-server] plan 10-10b: libbinder RpcServer %s on %s\n", argv[2], argv[1]);

    ABinderProcess_setThreadPoolMaxThreadCount(2);
    ABinderProcess_startThreadPool();

    int sockfd = bind_uds_listen(argv[1]);
    if (sockfd < 0) return 1;
    AIBinder_Class* root_clazz =
            AIBinder_Class_define(kRootDescriptor, on_create, on_destroy, root_on_transact);
    if (!root_clazz) return 3;
    AIBinder* root_binder = AIBinder_new(root_clazz, nullptr);
    if (!root_binder) return 4;
    ARpcServer* server = ARpcServer_newBoundSocket(root_binder, sockfd);
    if (!server) return 5;
    // Room for a client's founding connection plus one incoming connection.
    ARpcServer_setMaxThreads(server, 2);
    fprintf(stderr, "[cpp-server] READY\n");
    ARpcServer_join(server);
    AIBinder_decStrong(root_binder);
    return 0;
}

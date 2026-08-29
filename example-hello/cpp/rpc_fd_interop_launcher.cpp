// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-11 AC-11.3 STAGE3 — a real libbinder `ARpcSession` client with
// `ARpcSession_setFileDescriptorTransportMode(Unix)` driving an fd in
// both directions against the rsbinder `rpc_fd_interop_server`:
//
//   (arg)   TX_FD_LEN: we write a `ParcelFileDescriptor` (NDK
//           `AParcel_writeParcelFileDescriptor` ⇒ AOSP v1+
//           `[not-null|hasComm|TYPE|fdIndex]` body + SCM_RIGHTS); the
//           server reads the file and replies its byte length.
//   (reply) TX_GIVE_FD: the server writes a `ParcelFileDescriptor`; we
//           `AParcel_readParcelFileDescriptor` it and read its payload.
//
// Prints `FD_PASS` and exits 0 when both directions round-trip.
//
// Build (see run_rpc_fd_interop.sh):
//   $NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android35-clang++ \
//       -O2 -Wall -std=c++17 -static-libstdc++ \
//       -L /tmp -lbinder_ndk -lbinder_rpc_unstable -llog \
//       rpc_fd_interop_launcher.cpp -o rpc_fd_interop_launcher

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <memory>
#include <string>

extern "C" {
// --- libbinder_rpc_unstable.so (AOSP binder_rpc_unstable.hpp) ---
struct ARpcSession;
enum class ARpcSession_FileDescriptorTransportMode { None, Unix, Trusty };
ARpcSession* ARpcSession_new();
void ARpcSession_setMaxIncomingThreads(ARpcSession* session, size_t threads);
void ARpcSession_setMaxOutgoingConnections(ARpcSession* session, size_t connections);
void ARpcSession_setFileDescriptorTransportMode(ARpcSession* session,
                                                ARpcSession_FileDescriptorTransportMode mode);
// `ARpcSession_setupUnixDomainClient` takes an init-created socket *name*
// under /dev/socket, so a path in /data/local/tmp goes through the
// preconnected form with our own `connect(2)` instead.
AIBinder* ARpcSession_setupPreconnectedClient(ARpcSession* session,
                                              int (*requestFd)(void* param),
                                              void* param,
                                              void (*paramDeleteFd)(void* param));
void ARpcSession_free(ARpcSession* session);
}

namespace {

constexpr const char* kRootDescriptor = "rsbinder.test.IFdInterop";
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_FD_LEN = FIRST_CALL_TRANSACTION + 1;
constexpr transaction_code_t TX_GIVE_FD = FIRST_CALL_TRANSACTION + 2;
constexpr const char* kArgPayload = "hello-from-real-libbinder";
constexpr const char* kExpectedReplyPayload = "from-rsbinder-fd-2-11";

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

struct ParcelOwned {
    AParcel* p = nullptr;
    ~ParcelOwned() {
        if (p) AParcel_delete(p);
    }
};

// One `connect(2)` per requested connection (the initial one included).
int request_fd(void* param) {
    const char* path = static_cast<const char*>(param);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("[cpp-client] socket");
        return -1;
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (strlen(path) >= sizeof(addr.sun_path)) {
        fprintf(stderr, "[cpp-client] sock path too long\n");
        close(fd);
        return -1;
    }
    strncpy(addr.sun_path, path, sizeof(addr.sun_path) - 1);
    socklen_t len = (socklen_t)(offsetof(sockaddr_un, sun_path) + strlen(path) + 1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), len) < 0) {
        perror("[cpp-client] connect");
        close(fd);
        return -1;
    }
    fprintf(stderr, "[cpp-client] connected fd=%d (%s)\n", fd, path);
    return fd;
}
void delete_param(void* /*param*/) {}

void* on_create(void* args) { return args; }
void on_destroy(void* /*userData*/) {}
binder_status_t on_transact(AIBinder*, transaction_code_t, const AParcel*, AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

// An unlinked read/write file holding `payload`, positioned at 0.
int payload_fd(const char* payload) {
    char path[] = "/data/local/tmp/rsfd_arg_XXXXXX";
    int fd = mkstemp(path);
    if (fd < 0) {
        perror("[cpp-client] mkstemp");
        return -1;
    }
    unlink(path);
    size_t n = strlen(payload);
    if (write(fd, payload, n) != (ssize_t)n || lseek(fd, 0, SEEK_SET) != 0) {
        perror("[cpp-client] write payload");
        close(fd);
        return -1;
    }
    return fd;
}

bool do_echo(AIBinder* root, const char* arg, std::string* out) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return false;
    if (AParcel_writeString(in.p, arg, (int32_t)strlen(arg)) != STATUS_OK) return false;
    binder_status_t rc = AIBinder_transact(root, TX_ECHO, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(echo): %d\n", rc);
        return false;
    }
    if (AParcel_readString(reply.p, out, read_string_into) != STATUS_OK) return false;
    if (!out->empty() && out->back() == '\0') out->pop_back();
    return true;
}

bool do_fd_len(AIBinder* root, int fd, int32_t* out_len) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return false;
    binder_status_t rc = AParcel_writeParcelFileDescriptor(in.p, fd);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] writeParcelFileDescriptor: %d\n", rc);
        return false;
    }
    rc = AIBinder_transact(root, TX_FD_LEN, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(fd_len): %d\n", rc);
        return false;
    }
    return AParcel_readInt32(reply.p, out_len) == STATUS_OK;
}

bool do_give_fd(AIBinder* root, std::string* out_payload) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return false;
    binder_status_t rc = AIBinder_transact(root, TX_GIVE_FD, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(give_fd): %d\n", rc);
        return false;
    }
    int fd = -1;
    rc = AParcel_readParcelFileDescriptor(reply.p, &fd);
    if (rc != STATUS_OK || fd < 0) {
        fprintf(stderr, "[cpp-client] readParcelFileDescriptor: rc=%d fd=%d\n", rc, fd);
        return false;
    }
    char buf[256];
    lseek(fd, 0, SEEK_SET);
    ssize_t n = read(fd, buf, sizeof(buf) - 1);
    close(fd);
    if (n < 0) {
        perror("[cpp-client] read give_fd");
        return false;
    }
    out_payload->assign(buf, (size_t)n);
    return true;
}

} // namespace

int main(int argc, char** argv) {
    const char* sock_path = (argc > 1) ? argv[1] : "/data/local/tmp/rsfd.sock";
    fprintf(stderr, "[cpp-client] AC-11.3 STAGE3 fd-over-rpc sock=%s\n", sock_path);

    ARpcSession* session_raw = ARpcSession_new();
    if (!session_raw) {
        fprintf(stderr, "[cpp-client] ARpcSession_new failed\n");
        return 1;
    }
    auto session_owner = std::unique_ptr<ARpcSession, decltype(&ARpcSession_free)>(
            session_raw, ARpcSession_free);
    ARpcSession* session = session_owner.get();
    ARpcSession_setMaxOutgoingConnections(session, 1);
    ARpcSession_setMaxIncomingThreads(session, 0);
    ARpcSession_setFileDescriptorTransportMode(session,
                                               ARpcSession_FileDescriptorTransportMode::Unix);

    AIBinder* root_raw = ARpcSession_setupPreconnectedClient(
            session, request_fd, const_cast<char*>(sock_path), delete_param);
    if (!root_raw) {
        fprintf(stderr, "[cpp-client] setupPreconnectedClient returned null\n");
        return 2;
    }
    auto root_owner = std::unique_ptr<AIBinder, decltype(&AIBinder_decStrong)>(
            root_raw, AIBinder_decStrong);
    AIBinder* root = root_owner.get();

    AIBinder_Class* clazz = AIBinder_Class_define(kRootDescriptor, on_create, on_destroy, on_transact);
    if (!clazz || !AIBinder_associateClass(root, clazz)) {
        fprintf(stderr, "[cpp-client] class define/associate failed\n");
        return 3;
    }

    std::string echoed;
    if (!do_echo(root, "sanity", &echoed) || echoed != "sanity") {
        fprintf(stderr, "[cpp-client] sanity echo failed: got=%s\n", echoed.c_str());
        return 4;
    }
    fprintf(stderr, "[cpp-client] sanity OK\n");

    int fd = payload_fd(kArgPayload);
    if (fd < 0) return 5;
    int32_t len = -1;
    bool ok = do_fd_len(root, fd, &len);
    close(fd);
    if (!ok || len != (int32_t)strlen(kArgPayload)) {
        fprintf(stderr, "[cpp-client] (arg) FAIL: fd_len=%d expected %zu\n", len,
                strlen(kArgPayload));
        return 6;
    }
    fprintf(stderr, "[cpp-client] (arg) PASS — server read %d bytes from our fd\n", len);

    std::string payload;
    if (!do_give_fd(root, &payload) || payload != kExpectedReplyPayload) {
        fprintf(stderr, "[cpp-client] (reply) FAIL: payload=%s\n", payload.c_str());
        return 7;
    }
    fprintf(stderr, "[cpp-client] (reply) PASS — read %zu bytes from the server's fd\n",
            payload.size());

    printf("FD_PASS\n");
    return 0;
}

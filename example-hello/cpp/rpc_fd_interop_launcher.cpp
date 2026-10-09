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
// Holder relay (after FD_PASS): we send a server binder B back inside
// ParcelableHolder bytes (`[stability][size][payload]`, written by hand:
// the NDK holder is built on `AParcel_create` + `AParcel_appendFrom`, a
// kernel parcel that AOSP refuses to append to an RPC one), the server
// writes the undecoded holder into its reply, and we read B out of it.
// Wire v2 must relay (B comes back as the same proxy); older wires must
// answer BAD_TYPE as a status. Either way the session must stay usable
// (calls on B, the relayed B and the root succeed afterwards). Prints
// `HOLDER relay=<ok|bad_type> ...` and `HOLDER_PASS`.
//
// The server's live node count gates only a server binder that we drop
// without transacting on it or sending it back: its DEC_STRONG must free
// the node. B's own count is printed, and gated by the harness only on v2:
// B travels inside holder bytes the server never decodes, and below v2
// nothing enters a binder that is never read, so that send is not paid
// back (android-16.0.0_r4 `Parcel::rpcSetDataReference` enters every
// binder only on v2).
//
// Received-binder accounting (plan 2-23), after HOLDER_PASS: libbinder's
// `RpcState::transact` and a proxy sent back both run `onBinderLeaving` on
// our proxy (`timesSent++`, `sentRef` holds the BpBinder), and only a
// DEC_STRONG from the owner releases it (AOSP `flushExcessBinderRefs`).
// Three server binders are dropped after (TXN) transactions on them,
// (HOME) being sent back as an argument, and (RETRY) being sent inside a
// holder the server decodes twice, failing past the binder both times.
// Each must bring the server's node count back to its base. Prints `REF ...`
// and `REF_PASS`; the harness also greps logcat for an over-decrement.
//
// Null binder, after REF_PASS: AOSP writes a stability `int32` after a null
// RPC binder too (`flattenBinder` → `finishFlattenBinder`). We send a null
// binder and a sentinel; the server must read both, and its reply (a null
// binder, its verdict, the sentinel) must parse here. Prints `NULL ...` and
// `NULL_PASS`.
//
// Build (see run_rpc_fd_interop.sh):
//   $NDK/toolchains/llvm/prebuilt/darwin-x86_64/bin/aarch64-linux-android35-clang++ \
//       -O2 -Wall -std=c++17 -static-libstdc++ \
//       -L <pulled libbinder dir> -lbinder_ndk -lbinder_rpc_unstable -llog \
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
constexpr transaction_code_t TX_NEW_CHILD = FIRST_CALL_TRANSACTION + 3;
constexpr transaction_code_t TX_ECHO_HOLDER = FIRST_CALL_TRANSACTION + 4;
constexpr transaction_code_t TX_NODE_COUNT = FIRST_CALL_TRANSACTION + 5;
constexpr transaction_code_t TX_TAKE_BINDER = FIRST_CALL_TRANSACTION + 6;
constexpr transaction_code_t TX_HOLDER_RETRY = FIRST_CALL_TRANSACTION + 7;
constexpr const char* kRetryProbe = "rsbinder.test.RetryProbe";
constexpr transaction_code_t TX_NULL_BINDER = FIRST_CALL_TRANSACTION + 8;
constexpr int32_t kNullSentinel = 0x4e554c4c;
constexpr int32_t kHolderHead = 0x484f4c44;
constexpr int32_t kHolderTail = 0x21544c48;

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

bool do_node_count(AIBinder* root, int32_t* out) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return false;
    binder_status_t rc = AIBinder_transact(root, TX_NODE_COUNT, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(node_count): %d\n", rc);
        return false;
    }
    return AParcel_readInt32(reply.p, out) == STATUS_OK;
}

// A fresh server binder (+1 ref); a non-null `clazz` is associated (one INTERFACE_TRANSACTION).
AIBinder* do_new_child(AIBinder* root, const AIBinder_Class* clazz) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return nullptr;
    binder_status_t rc = AIBinder_transact(root, TX_NEW_CHILD, &in.p, &reply.p, 0);
    in.p = nullptr;
    AIBinder* child = nullptr;
    if (rc != STATUS_OK || AParcel_readStrongBinder(reply.p, &child) != STATUS_OK || !child) {
        fprintf(stderr, "[cpp-client] new_child: rc=%d child=%p\n", rc, child);
        return nullptr;
    }
    if (clazz && !AIBinder_associateClass(child, clazz)) {
        fprintf(stderr, "[cpp-client] new_child: associateClass failed\n");
        AIBinder_decStrong(child);
        return nullptr;
    }
    return child;
}

// Holder bytes `[stability=LOCAL][size][head, B, tail]`, as ParcelableHolder::writeToParcel
// lays out a parcel payload. Returns the transact status; on OK `*out` is B read back (+1 ref).
binder_status_t do_echo_holder(AIBinder* root, AIBinder* b, AIBinder** out) {
    ParcelOwned in, reply;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) return rc;
    AParcel_writeInt32(in.p, 0);
    int32_t size_pos = AParcel_getDataPosition(in.p);
    AParcel_writeInt32(in.p, 0);
    int32_t start = AParcel_getDataPosition(in.p);
    AParcel_writeInt32(in.p, kHolderHead);
    rc = AParcel_writeStrongBinder(in.p, b);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] echo_holder: writeStrongBinder: %d\n", rc);
        return rc;
    }
    AParcel_writeInt32(in.p, kHolderTail);
    int32_t end = AParcel_getDataPosition(in.p);
    AParcel_setDataPosition(in.p, size_pos);
    AParcel_writeInt32(in.p, end - start);
    AParcel_setDataPosition(in.p, end);

    rc = AIBinder_transact(root, TX_ECHO_HOLDER, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) return rc;

    int32_t stability = -1, size = -1, head = 0, tail = 0;
    AIBinder* got = nullptr;
    if (AParcel_readInt32(reply.p, &stability) != STATUS_OK ||
        AParcel_readInt32(reply.p, &size) != STATUS_OK ||
        AParcel_readInt32(reply.p, &head) != STATUS_OK ||
        AParcel_readStrongBinder(reply.p, &got) != STATUS_OK ||
        AParcel_readInt32(reply.p, &tail) != STATUS_OK) {
        fprintf(stderr, "[cpp-client] echo_holder: reply parse failed\n");
        if (got) AIBinder_decStrong(got);
        return STATUS_UNKNOWN_ERROR;
    }
    if (stability != 0 || size != end - start || head != kHolderHead || tail != kHolderTail) {
        fprintf(stderr, "[cpp-client] echo_holder: reply layout stab=%d size=%d head=%x tail=%x\n",
                stability, size, head, tail);
        if (got) AIBinder_decStrong(got);
        return STATUS_UNKNOWN_ERROR;
    }
    *out = got;
    return STATUS_OK;
}

// The holder relay case (file comment); returns 0 on HOLDER_PASS.
int run_holder_relay(AIBinder* root, const AIBinder_Class* clazz) {
    std::string echoed;
    int32_t base = -1, plain_live = -1, plain_after = -1, holder_live = -1, holder_after = -1;
    if (!do_node_count(root, &base)) return 20;

    // Never transacted on nor sent back, so our DEC_STRONG must free its node (file comment).
    AIBinder* p = do_new_child(root, nullptr);
    if (!p || !do_node_count(root, &plain_live)) return 21;
    AIBinder_decStrong(p);
    if (!do_node_count(root, &plain_after) || plain_live != base + 1 || plain_after != base) {
        fprintf(stderr, "[cpp-client] (plain) FAIL: base=%d live=%d after=%d\n", base, plain_live,
                plain_after);
        return 22;
    }

    AIBinder* b = do_new_child(root, clazz);
    if (!b || !do_node_count(root, &holder_live)) return 23;
    AIBinder* b2 = nullptr;
    binder_status_t rc = do_echo_holder(root, b, &b2);
    const char* relay = nullptr;
    if (rc == STATUS_OK && b2 == b) {
        relay = "ok";
    } else if (rc == STATUS_BAD_TYPE) {
        relay = "bad_type";
    } else {
        fprintf(stderr, "[cpp-client] (holder) FAIL: rc=%d b=%p b2=%p\n", rc, b, b2);
        if (b2) AIBinder_decStrong(b2);
        AIBinder_decStrong(b);
        return 24;
    }
    // The session must survive either outcome: call B, the relayed B, and the root.
    bool calls_ok = do_echo(b, "after-relay-b", &echoed) && echoed == "after-relay-b";
    if (b2) calls_ok = calls_ok && do_echo(b2, "after-relay-b2", &echoed);
    calls_ok = calls_ok && do_echo(root, "after-relay-root", &echoed);
    if (b2) AIBinder_decStrong(b2);
    AIBinder_decStrong(b);
    if (!calls_ok || !do_node_count(root, &holder_after)) {
        fprintf(stderr, "[cpp-client] (holder) FAIL: a call after the relay failed\n");
        return 25;
    }

    // holder_after is reported, not gated: B's proxy outlives our drop (file comment).
    printf("HOLDER relay=%s base=%d plain_live=%d plain_after=%d holder_live=%d holder_after=%d\n",
           relay, base, plain_live, plain_after, holder_live, holder_after);
    printf("HOLDER_PASS\n");
    return 0;
}

// Sends `b` as a plain argument (TX_TAKE_BINDER) or as the binder of a `RetryProbe` holder
// payload `[name, b, one i32]` (TX_HOLDER_RETRY); `*out` is the server's i32 answer.
bool do_send_binder(AIBinder* root, transaction_code_t code, AIBinder* b, int32_t* out) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return false;
    if (code == TX_HOLDER_RETRY) {
        AParcel_writeInt32(in.p, 0);
        int32_t size_pos = AParcel_getDataPosition(in.p);
        AParcel_writeInt32(in.p, 0);
        int32_t start = AParcel_getDataPosition(in.p);
        AParcel_writeString(in.p, kRetryProbe, (int32_t)strlen(kRetryProbe));
        if (AParcel_writeStrongBinder(in.p, b) != STATUS_OK) return false;
        AParcel_writeInt32(in.p, kHolderTail);
        int32_t end = AParcel_getDataPosition(in.p);
        AParcel_setDataPosition(in.p, size_pos);
        AParcel_writeInt32(in.p, end - start);
        AParcel_setDataPosition(in.p, end);
    } else if (AParcel_writeStrongBinder(in.p, b) != STATUS_OK) {
        return false;
    }
    binder_status_t rc = AIBinder_transact(root, code, &in.p, &reply.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(%u): %d\n", code, rc);
        return false;
    }
    return AParcel_readInt32(reply.p, out) == STATUS_OK;
}

// The received-binder accounting case (file comment); returns 0 on REF_PASS.
int run_ref_accounting(AIBinder* root, const AIBinder_Class* clazz) {
    std::string echoed;
    int32_t base = -1, txn_live = -1, txn_after = -1, home = -1, home_after = -1;
    int32_t retried = -1, retry_after = -1;
    if (!do_node_count(root, &base)) return 30;

    // TXN: associating the class is one INTERFACE_TRANSACTION, then three echoes.
    AIBinder* c = do_new_child(root, clazz);
    if (!c) return 31;
    for (int i = 0; i < 3; i++) {
        if (!do_echo(c, "ref-txn", &echoed) || echoed != "ref-txn") {
            AIBinder_decStrong(c);
            return 32;
        }
    }
    if (!do_node_count(root, &txn_live)) return 33;
    AIBinder_decStrong(c);
    if (!do_node_count(root, &txn_after)) return 34;

    AIBinder* d = do_new_child(root, nullptr);
    if (!d) return 35;
    bool sent = do_send_binder(root, TX_TAKE_BINDER, d, &home);
    AIBinder_decStrong(d);
    if (!sent || !do_node_count(root, &home_after)) return 36;

    AIBinder* h = do_new_child(root, nullptr);
    if (!h) return 37;
    sent = do_send_binder(root, TX_HOLDER_RETRY, h, &retried);
    AIBinder_decStrong(h);
    if (!sent || !do_node_count(root, &retry_after)) return 38;

    bool root_ok = do_echo(root, "after-ref", &echoed) && echoed == "after-ref";
    printf("REF base=%d txn_live=%d txn_after=%d home=%d home_after=%d retried=%d "
           "retry_after=%d root_ok=%d\n",
           base, txn_live, txn_after, home, home_after, retried, retry_after, root_ok);
    if (txn_live != base + 1 || txn_after != base || home != 1 || home_after != base ||
        retried != 1 || retry_after != base || !root_ok) {
        printf("REF_FAIL\n");
        return 39;
    }
    printf("REF_PASS\n");
    return 0;
}

// The null binder case (file comment); returns 0 on NULL_PASS.
int run_null_binder(AIBinder* root) {
    ParcelOwned in, reply;
    if (AIBinder_prepareTransaction(root, &in.p) != STATUS_OK) return 40;
    if (AParcel_writeStrongBinder(in.p, nullptr) != STATUS_OK ||
        AParcel_writeInt32(in.p, kNullSentinel) != STATUS_OK) {
        return 41;
    }
    binder_status_t rc = AIBinder_transact(root, TX_NULL_BINDER, &in.p, &reply.p, 0);
    in.p = nullptr;
    AIBinder* got = nullptr;
    int32_t server_ok = -1, sentinel = 0;
    binder_status_t rb = rc == STATUS_OK ? AParcel_readStrongBinder(reply.p, &got) : rc;
    binder_status_t r1 = rb == STATUS_OK ? AParcel_readInt32(reply.p, &server_ok) : rb;
    binder_status_t r2 = r1 == STATUS_OK ? AParcel_readInt32(reply.p, &sentinel) : r1;
    if (got) AIBinder_decStrong(got);
    std::string echoed;
    bool root_ok = do_echo(root, "after-null", &echoed) && echoed == "after-null";
    printf("NULL rc=%d read=%d server_ok=%d reply_null=%d sentinel=%#x root_ok=%d\n", rc, r2,
           server_ok, got == nullptr, sentinel, root_ok);
    if (r2 != STATUS_OK || server_ok != 1 || got != nullptr || sentinel != kNullSentinel ||
        !root_ok) {
        printf("NULL_FAIL\n");
        return 42;
    }
    printf("NULL_PASS\n");
    return 0;
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
    fflush(stdout);

    int rc = run_holder_relay(root, clazz);
    if (rc != 0) return rc;
    fflush(stdout);
    rc = run_ref_accounting(root, clazz);
    if (rc != 0) return rc;
    fflush(stdout);
    return run_null_binder(root);
}

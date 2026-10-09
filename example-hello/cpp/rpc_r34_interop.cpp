// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-25 Phase 7 STAGE3 — the real android-12 libbinder half of the
// r34 wire interop gate. Pairs with
// `example-hello/src/bin/rpc_r34_interop_{server,client}.rs`;
// `run_rpc_r34_interop.sh` builds, pushes and drives both on an SDK 31
// emulator.
//
// Android 12 has no RPC C API (`libbinder_rpc_unstable` arrived in 13), so
// this calls the platform C++ `RpcServer` / `RpcSession` exported by the
// device's libbinder.so (symbol version `LIBBINDER`) and crosses to the NDK
// with `AIBinder_{from,to}PlatformBinder`. Every transaction goes through the
// NDK `AIBinder`/`AParcel` API, so no std object of the NDK's libc++
// (`std::__ndk1`) crosses into the platform's (`std::__1`): the only platform
// calls take or return `sp<>`, `const char*`, integers and `bool`.
// The headers are android-12.0.0_r34's, so `sp<>` (returned through a hidden
// sret pointer) has the device's layout.
//
// Modes:
//   client <sock> <noroot_sock>  gate A: android-12 client → rsbinder server
//   client-bigreply <sock>       gate A, observational: a reply past 100,000 B
//   server <sock> <max_threads>  gate B: android-12 server for the rsbinder
//                                client; serves until killed
//
// Exit 0 means every check of the mode passed; each failing check returns its
// own code and prints `FAIL`.

#include <binder/IBinder.h>
#include <binder/RpcServer.h>
#include <binder/RpcSession.h>

#include <android/binder_ibinder.h>
#include <android/binder_libbinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <mutex>
#include <string>
#include <thread>
#include <vector>

using android::IBinder;
using android::RpcServer;
using android::RpcSession;
using android::sp;

namespace {

// Must match `rpc_r34_interop_{server,client}.rs`.
constexpr const char* kRootDescriptor = "rsbinder.test.IR34Interop";
constexpr const char* kCallbackDescriptor = "rsbinder.test.IR34Callback";
constexpr const char* kChildDescriptor = "rsbinder.test.IR34Child";

constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_SLOW_ECHO = FIRST_CALL_TRANSACTION + 1;
constexpr transaction_code_t TX_ONEWAY = FIRST_CALL_TRANSACTION + 2;
constexpr transaction_code_t TX_GET_LOG = FIRST_CALL_TRANSACTION + 3;
constexpr transaction_code_t TX_INVOKE_CALLBACK = FIRST_CALL_TRANSACTION + 4;
constexpr transaction_code_t TX_NULL_BINDER = FIRST_CALL_TRANSACTION + 5;
constexpr transaction_code_t TX_HOLD = FIRST_CALL_TRANSACTION + 6;
constexpr transaction_code_t TX_RELEASE = FIRST_CALL_TRANSACTION + 7;
constexpr transaction_code_t TX_ECHO_BINDER = FIRST_CALL_TRANSACTION + 8;
constexpr transaction_code_t TX_GET_CHILD = FIRST_CALL_TRANSACTION + 9;
constexpr transaction_code_t TX_STATS = FIRST_CALL_TRANSACTION + 10;
constexpr transaction_code_t TX_BYTES = FIRST_CALL_TRANSACTION + 11;
constexpr transaction_code_t TX_ONEWAY_BINDER = FIRST_CALL_TRANSACTION + 12;
constexpr transaction_code_t TX_CB_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_CHILD_PING = FIRST_CALL_TRANSACTION + 0;

#define CHECK(cond, code, ...)                                   \
    do {                                                         \
        if (!(cond)) {                                           \
            fprintf(stderr, "[cpp] FAIL (%d): ", (code));        \
            fprintf(stderr, __VA_ARGS__);                        \
            fprintf(stderr, "\n");                               \
            return (code);                                       \
        }                                                        \
    } while (0)

// ---------------------------------------------------------------- parcels

struct Parcel {
    AParcel* p = nullptr;
    ~Parcel() {
        if (p) AParcel_delete(p);
    }
};
struct Status {
    AStatus* s = nullptr;
    ~Status() {
        if (s) AStatus_delete(s);
    }
};
struct Binder {
    AIBinder* b = nullptr;
    Binder() = default;
    explicit Binder(AIBinder* b) : b(b) {}
    Binder(const Binder&) = delete;
    Binder& operator=(const Binder&) = delete;
    ~Binder() { reset(); }
    void reset() {
        if (b) AIBinder_decStrong(b);
        b = nullptr;
    }
};

bool string_alloc(void* opaque, int32_t length, char** out) {
    auto* s = static_cast<std::string*>(opaque);
    if (length < 0) {
        *out = nullptr;
        return true;
    }
    s->resize(length);
    *out = s->data();
    return true;
}

bool bytes_alloc(void* opaque, int32_t length, int8_t** out) {
    auto* v = static_cast<std::vector<int8_t>*>(opaque);
    if (length < 0) {
        *out = nullptr;
        return true;
    }
    v->resize(length);
    *out = v->data();
    return true;
}

binder_status_t read_string(const AParcel* p, std::string* out) {
    out->clear();
    binder_status_t rc = AParcel_readString(p, out, string_alloc);
    if (rc == STATUS_OK && !out->empty() && out->back() == '\0') out->pop_back();
    return rc;
}

binder_status_t write_string(AParcel* p, const std::string& s) {
    return AParcel_writeString(p, s.c_str(), (int32_t)s.size());
}

binder_status_t write_ok(AParcel* out) {
    Status st{AStatus_newOk()};
    return AParcel_writeStatusHeader(out, st.s);
}

// --------------------------------------------------------------- classes

// Destruction witness for callbacks: `onDestroy` receives the user data
// `AIBinder_new` was given, a counter here.
void* on_create(void* args) { return args; }
void on_destroy(void* user) {
    if (user) static_cast<std::atomic<int>*>(user)->fetch_add(1);
}

binder_status_t callback_on_transact(AIBinder*, transaction_code_t code, const AParcel* in,
                                     AParcel* out) {
    if (code != TX_CB_ECHO) return STATUS_UNKNOWN_TRANSACTION;
    std::string s;
    binder_status_t rc = read_string(in, &s);
    if (rc != STATUS_OK) return rc;
    if ((rc = write_ok(out)) != STATUS_OK) return rc;
    return write_string(out, "cb-echo:" + s);
}

binder_status_t root_on_transact(AIBinder*, transaction_code_t, const AParcel*, AParcel*);

binder_status_t client_root_on_transact(AIBinder*, transaction_code_t, const AParcel*, AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

// One class object per descriptor: android-12 `associateClass` refuses a
// second class object of the same descriptor.
AIBinder_Class* g_callback_class = nullptr;
AIBinder_Class* g_child_class = nullptr;
AIBinder_Class* g_root_class = nullptr;

void define_classes(bool server) {
    g_callback_class = AIBinder_Class_define(kCallbackDescriptor, on_create, on_destroy,
                                             callback_on_transact);
    g_child_class = AIBinder_Class_define(kChildDescriptor, on_create, on_destroy,
                                          client_root_on_transact);
    g_root_class = AIBinder_Class_define(kRootDescriptor, on_create, on_destroy,
                                         server ? root_on_transact : client_root_on_transact);
}

// ------------------------------------------------------------ transactions

// `TX_*` on `target` with `fill` writing the arguments; returns the
// transport status, then (twoway) checks and strips the status header.
template <typename Fill>
binder_status_t call(AIBinder* target, transaction_code_t code, Fill fill, Parcel* reply,
                     binder_flags_t flags = 0) {
    Parcel in;
    binder_status_t rc = AIBinder_prepareTransaction(target, &in.p);
    if (rc != STATUS_OK) return rc;
    if ((rc = fill(in.p)) != STATUS_OK) return rc;
    rc = AIBinder_transact(target, code, &in.p, &reply->p, flags);
    in.p = nullptr;
    if (rc != STATUS_OK || (flags & FLAG_ONEWAY)) return rc;
    Status st;
    rc = AParcel_readStatusHeader(reply->p, &st.s);
    if (rc != STATUS_OK) return rc;
    if (!AStatus_isOk(st.s)) {
        fprintf(stderr, "[cpp] code %u: remote status %s\n", code, AStatus_getDescription(st.s));
        return STATUS_FAILED_TRANSACTION;
    }
    return STATUS_OK;
}

const auto kNoArgs = [](AParcel*) { return STATUS_OK; };

binder_status_t echo(AIBinder* root, const std::string& s, std::string* out) {
    Parcel r;
    binder_status_t rc = call(root, TX_ECHO, [&](AParcel* p) { return write_string(p, s); }, &r);
    if (rc != STATUS_OK) return rc;
    return read_string(r.p, out);
}

binder_status_t slow_echo(AIBinder* root, const std::string& s, int32_t ms, std::string* out) {
    Parcel r;
    binder_status_t rc = call(
            root, TX_SLOW_ECHO,
            [&](AParcel* p) {
                binder_status_t rc = write_string(p, s);
                return rc == STATUS_OK ? AParcel_writeInt32(p, ms) : rc;
            },
            &r);
    if (rc != STATUS_OK) return rc;
    return read_string(r.p, out);
}

binder_status_t get_log(AIBinder* root, std::vector<int32_t>* out) {
    Parcel r;
    binder_status_t rc = call(root, TX_GET_LOG, kNoArgs, &r);
    if (rc != STATUS_OK) return rc;
    int32_t n = 0;
    if ((rc = AParcel_readInt32(r.p, &n)) != STATUS_OK) return rc;
    out->assign(n < 0 ? 0 : n, 0);
    for (int32_t i = 0; i < n; ++i) {
        if ((rc = AParcel_readInt32(r.p, &(*out)[i])) != STATUS_OK) return rc;
    }
    return STATUS_OK;
}

binder_status_t invoke_callback(AIBinder* root, AIBinder* cb, const std::string& s,
                                std::string* out) {
    Parcel r;
    binder_status_t rc = call(
            root, TX_INVOKE_CALLBACK,
            [&](AParcel* p) {
                binder_status_t rc = AParcel_writeStrongBinder(p, cb);
                return rc == STATUS_OK ? write_string(p, s) : rc;
            },
            &r);
    if (rc != STATUS_OK) return rc;
    return read_string(r.p, out);
}

binder_status_t send_binder(AIBinder* root, transaction_code_t code, AIBinder* b,
                            binder_flags_t flags = 0) {
    Parcel r;
    return call(root, code, [&](AParcel* p) { return AParcel_writeStrongBinder(p, b); }, &r,
                flags);
}

binder_status_t release(AIBinder* root, int32_t* released) {
    Parcel r;
    binder_status_t rc = call(root, TX_RELEASE, kNoArgs, &r);
    if (rc != STATUS_OK) return rc;
    return AParcel_readInt32(r.p, released);
}

// `TX_STATS` → rsbinder server counters, in this order.
struct Stats {
    int32_t nodes = -1, registered = -1, attached = -1, rejected = -1;
};
binder_status_t stats(AIBinder* root, Stats* s) {
    Parcel r;
    binder_status_t rc = call(root, TX_STATS, kNoArgs, &r);
    if (rc != STATUS_OK) return rc;
    for (int32_t* f : {&s->nodes, &s->registered, &s->attached, &s->rejected}) {
        if ((rc = AParcel_readInt32(r.p, f)) != STATUS_OK) return rc;
    }
    return STATUS_OK;
}

// `TX_BYTES(reply_len, payload)` → `(payload.len(), byte[reply_len])`.
binder_status_t bytes(AIBinder* root, int32_t payload_len, int32_t reply_len, int32_t* seen,
                      std::vector<int8_t>* out) {
    std::vector<int8_t> payload(payload_len, 0x5a);
    Parcel r;
    binder_status_t rc = call(
            root, TX_BYTES,
            [&](AParcel* p) {
                binder_status_t rc = AParcel_writeInt32(p, reply_len);
                return rc == STATUS_OK ? AParcel_writeByteArray(p, payload.data(), payload_len)
                                       : rc;
            },
            &r);
    if (rc != STATUS_OK) return rc;
    if ((rc = AParcel_readInt32(r.p, seen)) != STATUS_OK) return rc;
    return AParcel_readByteArray(r.p, out, bytes_alloc);
}

template <typename Pred>
bool poll_until(int timeout_ms, Pred pred) {
    for (int waited = 0; waited <= timeout_ms; waited += 20) {
        if (pred()) return true;
        std::this_thread::sleep_for(std::chrono::milliseconds(20));
    }
    return false;
}

long long ms_since(std::chrono::steady_clock::time_point t) {
    return std::chrono::duration_cast<std::chrono::milliseconds>(
                   std::chrono::steady_clock::now() - t)
            .count();
}

// Connects an android-12 client session and returns its root as an NDK
// binder associated with the root class (null on failure).
AIBinder* connect_root(const char* sock, sp<RpcSession>* session, size_t* max_threads) {
    *session = RpcSession::make();
    if (!(*session)->setupUnixDomainClient(sock)) {
        fprintf(stderr, "[cpp] setupUnixDomainClient(%s) failed\n", sock);
        return nullptr;
    }
    if ((*session)->getRemoteMaxThreads(max_threads) != android::OK) {
        fprintf(stderr, "[cpp] getRemoteMaxThreads failed\n");
        return nullptr;
    }
    sp<IBinder> root = (*session)->getRootObject();
    if (root == nullptr) {
        fprintf(stderr, "[cpp] getRootObject returned null\n");
        return nullptr;
    }
    AIBinder* b = AIBinder_fromPlatformBinder(root);
    if (!b || !AIBinder_associateClass(b, g_root_class)) {
        fprintf(stderr, "[cpp] root: fromPlatformBinder/associateClass failed\n");
        if (b) AIBinder_decStrong(b);
        return nullptr;
    }
    return b;
}

// ------------------------------------------------------------ gate A client

int run_client(const char* sock, const char* noroot_sock) {
    define_classes(false);

    // (A1) three connections: GET_MAX_THREADS = 3, so android-12 opens the
    // founding connection plus two that write the session id.
    sp<RpcSession> session;
    size_t max_threads = 0;
    Binder root(connect_root(sock, &session, &max_threads));
    CHECK(root.b, 10, "connect to %s", sock);
    CHECK(max_threads == 3, 11, "remote max threads %zu, want 3", max_threads);
    std::string got;
    CHECK(echo(root.b, "sanity", &got) == STATUS_OK && got == "sanity", 12, "echo got '%s'",
          got.c_str());
    // android-12 writes the id on a joined connection and does not wait for
    // the server to take it, so the server may not have accepted both yet.
    Stats st;
    bool joined = poll_until(3000, [&]() {
        return stats(root.b, &st) == STATUS_OK && st.attached >= 2;
    });
    fprintf(stderr, "[cpp] stats: nodes=%d registered=%d attached=%d rejected=%d\n", st.nodes,
            st.registered, st.attached, st.rejected);
    CHECK(joined && st.registered == 1 && st.attached == 2 && st.rejected == 0, 14,
          "want 1 session with 2 joined connections");
    printf("(A1) PASS: 3 connections (1 founding + 2 joined), echo\n");

    // (A2) parallel twoway: three 150 ms calls, one per connection.
    {
        constexpr int kN = 3, kMs = 150;
        std::vector<std::string> out(kN);
        std::vector<binder_status_t> rcs(kN, -1);
        auto start = std::chrono::steady_clock::now();
        std::vector<std::thread> ths;
        for (int i = 0; i < kN; ++i) {
            ths.emplace_back([&, i]() {
                rcs[i] = slow_echo(root.b, "t" + std::to_string(i), kMs, &out[i]);
            });
        }
        for (auto& t : ths) t.join();
        long long elapsed = ms_since(start);
        for (int i = 0; i < kN; ++i) {
            CHECK(rcs[i] == STATUS_OK && out[i] == "t" + std::to_string(i), 20,
                  "slow_echo %d rc=%d got '%s'", i, rcs[i], out[i].c_str());
        }
        // Serial would take 450 ms.
        CHECK(elapsed < 2 * kMs, 21, "3 x %d ms took %lld ms: serialized", kMs, elapsed);
        printf("(A2) PASS: 3 parallel %d ms twoway calls in %lld ms\n", kMs, elapsed);
    }

    // (A3) oneway order: android-12 spreads oneway calls over its
    // connections (`CLIENT_ASYNC` advances the offset), so the server must
    // order them by asyncNumber.
    {
        constexpr int kN = 20;
        for (int i = 0; i < kN; ++i) {
            Parcel r;
            binder_status_t rc = call(
                    root.b, TX_ONEWAY, [&](AParcel* p) { return AParcel_writeInt32(p, i); }, &r,
                    FLAG_ONEWAY);
            CHECK(rc == STATUS_OK, 30, "oneway %d rc=%d", i, rc);
        }
        std::vector<int32_t> log;
        bool full = poll_until(3000, [&]() {
            return get_log(root.b, &log) == STATUS_OK && (int)log.size() >= kN;
        });
        CHECK(full, 31, "log has %zu of %d", log.size(), kN);
        for (int i = 0; i < kN; ++i) CHECK(log[i] == i, 32, "log[%d]=%d", i, log[i]);
        printf("(A3) PASS: %d oneway calls in order\n", kN);
    }

    // (A4) nested callback, alone and from three threads at once.
    std::atomic<int> cb_destroyed{0};
    Binder cb(AIBinder_new(g_callback_class, &cb_destroyed));
    CHECK(cb.b, 40, "AIBinder_new(callback)");
    CHECK(invoke_callback(root.b, cb.b, "ping", &got) == STATUS_OK && got == "cb-echo:ping", 41,
          "nested callback got '%s'", got.c_str());
    {
        constexpr int kN = 3;
        std::vector<std::string> out(kN);
        std::vector<binder_status_t> rcs(kN, -1);
        std::vector<std::thread> ths;
        for (int i = 0; i < kN; ++i) {
            ths.emplace_back([&, i]() {
                rcs[i] = invoke_callback(root.b, cb.b, "p" + std::to_string(i), &out[i]);
            });
        }
        for (auto& t : ths) t.join();
        for (int i = 0; i < kN; ++i) {
            CHECK(rcs[i] == STATUS_OK && out[i] == "cb-echo:p" + std::to_string(i), 42,
                  "parallel nested %d rc=%d got '%s'", i, rcs[i], out[i].c_str());
        }
    }
    printf("(A4) PASS: nested callback, single and 3 in parallel\n");

    // (A5) null binder in both directions, followed by an int: a missing
    // stability word would shift the int by 4 bytes.
    for (AIBinder* arg : {(AIBinder*)nullptr, cb.b}) {
        Parcel r;
        binder_status_t rc = call(
                root.b, TX_NULL_BINDER,
                [&](AParcel* p) {
                    binder_status_t rc = AParcel_writeStrongBinder(p, arg);
                    return rc == STATUS_OK ? AParcel_writeInt32(p, 41) : rc;
                },
                &r);
        CHECK(rc == STATUS_OK, 50, "null_binder rc=%d", rc);
        int32_t was_null = -1, next = -1;
        AIBinder* back = nullptr;
        CHECK(AParcel_readInt32(r.p, &was_null) == STATUS_OK, 51, "read was_null");
        CHECK(AParcel_readStrongBinder(r.p, &back) == STATUS_OK, 52, "read null binder");
        CHECK(AParcel_readInt32(r.p, &next) == STATUS_OK, 53, "read int after null binder");
        if (back) AIBinder_decStrong(back);
        CHECK(was_null == (arg == nullptr) && back == nullptr && next == 42, 54,
              "was_null=%d back=%p next=%d", was_null, (void*)back, next);
    }
    printf("(A5) PASS: null binder then int, both directions\n");

    // (A6) refcounts.
    {
        // The server hands back our own callback: android-12 resolves its
        // own address to the same local object.
        Parcel r;
        CHECK(call(root.b, TX_ECHO_BINDER,
                   [&](AParcel* p) { return AParcel_writeStrongBinder(p, cb.b); },
                   &r) == STATUS_OK,
              61, "echo_binder");
        AIBinder* back = nullptr;
        CHECK(AParcel_readStrongBinder(r.p, &back) == STATUS_OK, 62, "read echoed binder");
        Binder back_owned(back);
        CHECK(back == cb.b, 63, "echoed binder %p is not our callback %p", (void*)back,
              (void*)cb.b);

        // Same callback held twice by the server, then released: once our
        // own reference is gone too, android-12 destroys it only if the
        // server returned every reference it got.
        std::atomic<int> held_destroyed{0};
        Binder held(AIBinder_new(g_callback_class, &held_destroyed));
        CHECK(send_binder(root.b, TX_HOLD, held.b) == STATUS_OK, 64, "hold #1");
        CHECK(send_binder(root.b, TX_HOLD, held.b) == STATUS_OK, 64, "hold #2");
        held.reset();
        std::this_thread::sleep_for(std::chrono::milliseconds(100));
        CHECK(held_destroyed.load() == 0, 65, "callback destroyed while the server holds it");
        int32_t released = -1;
        CHECK(release(root.b, &released) == STATUS_OK && released == 2, 66, "released %d",
              released);
        bool gone = poll_until(3000, [&]() {
            std::string s;
            // Each twoway reads queued DEC_STRONGs on the connection it uses.
            std::vector<std::thread> ths;
            for (int i = 0; i < 3; ++i) {
                ths.emplace_back([&]() { slow_echo(root.b, "d", 20, &s); });
            }
            for (auto& t : ths) t.join();
            return held_destroyed.load() == 1;
        });
        CHECK(gone, 67, "held callback not destroyed after release");

        // A server binder received twice, then dropped: the server's node
        // count returns to where it was.
        Stats before;
        CHECK(stats(root.b, &before) == STATUS_OK, 68, "stats before child");
        AIBinder* c1 = nullptr;
        AIBinder* c2 = nullptr;
        for (AIBinder** c : {&c1, &c2}) {
            Parcel cr;
            CHECK(call(root.b, TX_GET_CHILD, kNoArgs, &cr) == STATUS_OK, 69, "get_child");
            CHECK(AParcel_readStrongBinder(cr.p, c) == STATUS_OK && *c, 69, "read child");
        }
        Binder o1(c1), o2(c2);
        CHECK(c1 == c2, 70, "the same child came back as two proxies");
        CHECK(AIBinder_associateClass(c1, g_child_class), 71, "child associateClass");
        Parcel pr;
        int32_t pong = -1;
        CHECK(call(c1, TX_CHILD_PING, kNoArgs, &pr) == STATUS_OK &&
                      AParcel_readInt32(pr.p, &pong) == STATUS_OK && pong == 7,
              72, "child ping %d", pong);
        Stats mid;
        CHECK(stats(root.b, &mid) == STATUS_OK && mid.nodes == before.nodes + 1, 73,
              "nodes %d -> %d with the child out", before.nodes, mid.nodes);
        o1.reset();
        o2.reset();
        Stats after;
        bool back_to_base = poll_until(3000, [&]() {
            return stats(root.b, &after) == STATUS_OK && after.nodes == before.nodes;
        });
        CHECK(back_to_base, 74, "nodes %d -> %d after dropping the child", before.nodes,
              after.nodes);

        // `cb` went out in (A4)-(A6) and came back once: android-12 answers
        // a receipt of its own binder with an immediate DEC_STRONG, so
        // nothing stays counted and dropping our reference destroys it.
        back_owned.reset();
        cb.reset();
        bool cb_gone = poll_until(3000, [&]() {
            std::string s;
            std::vector<std::thread> ths;
            for (int i = 0; i < 3; ++i) {
                ths.emplace_back([&]() { slow_echo(root.b, "c", 20, &s); });
            }
            for (auto& t : ths) t.join();
            return cb_destroyed.load() == 1;
        });
        CHECK(cb_gone, 75, "callback sent 6 times and echoed back once is never destroyed");
        printf("(A6) PASS: own binder echoed back, callbacks destroyed once released (held "
               "twice; sent 6 times), server nodes %d -> %d -> %d\n",
               before.nodes, mid.nodes, after.nodes);
    }

    // (A7) DEC_STRONG flood: 20,000 oneway calls each carrying the callback.
    // The server drops each proxy, so ~1 MB of DEC_STRONG heads back while
    // android-12 is still sending and reads nothing. The server must keep
    // reading while its own writes wait.
    {
        constexpr int kN = 20000;
        std::atomic<int> flood_destroyed{0};
        Binder fcb(AIBinder_new(g_callback_class, &flood_destroyed));
        auto start = std::chrono::steady_clock::now();
        for (int i = 0; i < kN; ++i) {
            binder_status_t rc = send_binder(root.b, TX_ONEWAY_BINDER, fcb.b, FLAG_ONEWAY);
            CHECK(rc == STATUS_OK, 76, "oneway binder %d rc=%d", i, rc);
        }
        long long send_ms = ms_since(start);
        fcb.reset();
        bool gone = poll_until(10000, [&]() {
            std::string s;
            std::vector<std::thread> ths;
            for (int i = 0; i < 3; ++i) {
                ths.emplace_back([&]() { slow_echo(root.b, "f", 20, &s); });
            }
            for (auto& t : ths) t.join();
            return flood_destroyed.load() == 1;
        });
        CHECK(gone, 77, "flood callback not destroyed");
        printf("(A7) PASS: %d oneway binder sends in %lld ms, callback destroyed after "
               "%lld ms\n",
               kN, send_ms, ms_since(start));
    }

    // (A8) sizes around android-12's body limit (`kMaxTransactionAllocation`,
    // 100,000 bytes).
    {
        int32_t seen = -1;
        std::vector<int8_t> out;
        binder_status_t rc = bytes(root.b, 90000, 0, &seen, &out);
        CHECK(rc == STATUS_OK && seen == 90000, 80, "90 KB request rc=%d seen=%d", rc, seen);
        rc = bytes(root.b, 0, 90000, &seen, &out);
        CHECK(rc == STATUS_OK && out.size() == 90000, 81, "90 KB reply rc=%d size=%zu", rc,
              out.size());
        // android-12 refuses to build a body past the limit and sends nothing.
        rc = bytes(root.b, 110000, 0, &seen, &out);
        CHECK(rc == STATUS_NO_MEMORY, 82, "110 KB request rc=%d, want NO_MEMORY", rc);
        CHECK(echo(root.b, "after", &got) == STATUS_OK && got == "after", 83,
              "session after refused send");
        printf("(A8) PASS: 90 KB request and reply; 110 KB request refused locally (NO_MEMORY), "
               "session intact\n");
    }

    // (A9) GET_ROOT on a server with no root: android-12 gets null.
    {
        sp<RpcSession> s2 = RpcSession::make();
        CHECK(s2->setupUnixDomainClient(noroot_sock), 90, "connect %s", noroot_sock);
        sp<IBinder> r2 = s2->getRootObject();
        CHECK(r2 == nullptr, 91, "root of a rootless server is not null");
        printf("(A9) PASS: GET_ROOT without a root object is null\n");
    }

    CHECK(stats(root.b, &st) == STATUS_OK, 95, "final stats");
    fprintf(stderr, "[cpp] final stats: nodes=%d registered=%d attached=%d rejected=%d\n",
            st.nodes, st.registered, st.attached, st.rejected);
    CHECK(st.rejected == 0, 96, "server refused %d connections", st.rejected);

    printf("GATE A PASS\n");
    fflush(stdout);
    return 0;
}

// A reply past 100,000 bytes. android-12 returns NO_MEMORY from
// `waitForReply` without reading the body, so the rest of that connection's
// byte stream is out of step. Observational: prints what follows.
int run_client_bigreply(const char* sock) {
    define_classes(false);
    sp<RpcSession> session;
    size_t max_threads = 0;
    Binder root(connect_root(sock, &session, &max_threads));
    CHECK(root.b, 100, "connect to %s", sock);
    // Let the server take both joined connections, so none is still queued
    // when the session ends at exit: three calls run at once only when all
    // three connections are served. (`attached` counts the whole server.)
    bool all_served = poll_until(3000, [&]() {
        std::vector<std::thread> ths;
        auto start = std::chrono::steady_clock::now();
        for (int i = 0; i < 3; ++i) {
            ths.emplace_back([&]() {
                std::string s;
                slow_echo(root.b, "j", 100, &s);
            });
        }
        for (auto& t : ths) t.join();
        return ms_since(start) < 200;
    });
    CHECK(all_served, 102, "joined connections not served");
    int32_t seen = -1;
    std::vector<int8_t> out;
    binder_status_t rc = bytes(root.b, 0, 110000, &seen, &out);
    printf("(A10) 110 KB reply: rc=%d (NO_MEMORY=%d)\n", rc, STATUS_NO_MEMORY);
    std::string got;
    binder_status_t rc2 = echo(root.b, "after-big", &got);
    printf("(A10) next echo: rc=%d got='%s'\n", rc2, got.c_str());
    fflush(stdout);
    CHECK(rc == STATUS_NO_MEMORY, 101, "110 KB reply rc=%d, want NO_MEMORY", rc);
    // The session's sockets close at exit; skip the destructors, which would
    // send DEC_STRONG on a connection that is out of step.
    _exit(0);
}

// -------------------------------------------------------------- gate B server

std::mutex g_mu;
std::vector<int32_t> g_log;
std::vector<AIBinder*> g_held;

binder_status_t root_on_transact(AIBinder*, transaction_code_t code, const AParcel* in,
                                 AParcel* out) {
    binder_status_t rc = STATUS_OK;
    switch (code) {
        case TX_ECHO: {
            std::string s;
            if ((rc = read_string(in, &s)) != STATUS_OK) return rc;
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            return write_string(out, s);
        }
        case TX_SLOW_ECHO: {
            std::string s;
            int32_t ms = 0;
            if ((rc = read_string(in, &s)) != STATUS_OK) return rc;
            if ((rc = AParcel_readInt32(in, &ms)) != STATUS_OK) return rc;
            std::this_thread::sleep_for(std::chrono::milliseconds(ms));
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            return write_string(out, s);
        }
        case TX_ONEWAY: {
            int32_t i = 0;
            if ((rc = AParcel_readInt32(in, &i)) != STATUS_OK) return rc;
            std::lock_guard<std::mutex> l(g_mu);
            g_log.push_back(i);
            return STATUS_OK;
        }
        case TX_GET_LOG: {
            std::vector<int32_t> log;
            {
                std::lock_guard<std::mutex> l(g_mu);
                log = g_log;
            }
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            if ((rc = AParcel_writeInt32(out, (int32_t)log.size())) != STATUS_OK) return rc;
            for (int32_t v : log) {
                if ((rc = AParcel_writeInt32(out, v)) != STATUS_OK) return rc;
            }
            return STATUS_OK;
        }
        case TX_INVOKE_CALLBACK: {
            AIBinder* raw = nullptr;
            if ((rc = AParcel_readStrongBinder(in, &raw)) != STATUS_OK) return rc;
            Binder cb(raw);
            std::string s;
            if ((rc = read_string(in, &s)) != STATUS_OK) return rc;
            if (!cb.b) return STATUS_UNEXPECTED_NULL;
            // Asks the rsbinder client for its descriptor (a nested
            // INTERFACE_TRANSACTION) before the nested call itself.
            if (!AIBinder_associateClass(cb.b, g_callback_class)) return STATUS_BAD_TYPE;
            Parcel r;
            rc = call(cb.b, TX_CB_ECHO, [&](AParcel* p) { return write_string(p, s); }, &r);
            if (rc != STATUS_OK) return rc;
            std::string cb_reply;
            if ((rc = read_string(r.p, &cb_reply)) != STATUS_OK) return rc;
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            return write_string(out, cb_reply);
        }
        case TX_NULL_BINDER: {
            AIBinder* raw = nullptr;
            int32_t x = 0;
            if ((rc = AParcel_readStrongBinder(in, &raw)) != STATUS_OK) return rc;
            Binder b(raw);
            if ((rc = AParcel_readInt32(in, &x)) != STATUS_OK) return rc;
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            if ((rc = AParcel_writeInt32(out, b.b == nullptr)) != STATUS_OK) return rc;
            if ((rc = AParcel_writeStrongBinder(out, nullptr)) != STATUS_OK) return rc;
            return AParcel_writeInt32(out, x + 1);
        }
        case TX_HOLD: {
            AIBinder* raw = nullptr;
            if ((rc = AParcel_readStrongBinder(in, &raw)) != STATUS_OK) return rc;
            {
                std::lock_guard<std::mutex> l(g_mu);
                g_held.push_back(raw);
            }
            return write_ok(out);
        }
        case TX_RELEASE: {
            std::vector<AIBinder*> held;
            {
                std::lock_guard<std::mutex> l(g_mu);
                held.swap(g_held);
            }
            for (AIBinder* b : held) {
                if (b) AIBinder_decStrong(b);
            }
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            return AParcel_writeInt32(out, (int32_t)held.size());
        }
        case TX_ECHO_BINDER: {
            AIBinder* raw = nullptr;
            if ((rc = AParcel_readStrongBinder(in, &raw)) != STATUS_OK) return rc;
            Binder b(raw);
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            return AParcel_writeStrongBinder(out, b.b);
        }
        case TX_BYTES: {
            int32_t reply_len = 0;
            std::vector<int8_t> payload;
            if ((rc = AParcel_readInt32(in, &reply_len)) != STATUS_OK) return rc;
            if ((rc = AParcel_readByteArray(in, &payload, bytes_alloc)) != STATUS_OK) return rc;
            std::vector<int8_t> reply(reply_len < 0 ? 0 : reply_len, 0x33);
            if ((rc = write_ok(out)) != STATUS_OK) return rc;
            if ((rc = AParcel_writeInt32(out, (int32_t)payload.size())) != STATUS_OK) return rc;
            return AParcel_writeByteArray(out, reply.data(), (int32_t)reply.size());
        }
        default:
            return STATUS_UNKNOWN_TRANSACTION;
    }
}

int run_server(const char* sock, size_t max_threads) {
    define_classes(true);
    AIBinder* root = AIBinder_new(g_root_class, nullptr);
    if (!root) {
        fprintf(stderr, "[cpp-server] AIBinder_new(root) failed\n");
        return 110;
    }
    sp<RpcServer> server = RpcServer::make();
    server->iUnderstandThisCodeIsExperimentalAndIWillNotUseItInProduction();
    server->setMaxThreads(max_threads);
    server->setRootObject(AIBinder_toPlatformBinder(root));
    unlink(sock);
    if (!server->setupUnixDomainServer(sock)) {
        fprintf(stderr, "[cpp-server] setupUnixDomainServer(%s) failed\n", sock);
        return 111;
    }
    printf("[cpp-server] READY sock=%s max_threads=%zu\n", sock, server->getMaxThreads());
    fflush(stdout);
    server->join();
    return 0;
}

} // namespace

int main(int argc, char** argv) {
    const char* mode = argc > 1 ? argv[1] : "";
    if (!strcmp(mode, "client") && argc == 4) return run_client(argv[2], argv[3]);
    if (!strcmp(mode, "client-bigreply") && argc == 3) return run_client_bigreply(argv[2]);
    if (!strcmp(mode, "server") && argc == 4) return run_server(argv[2], strtoul(argv[3], nullptr, 10));
    fprintf(stderr,
            "usage: %s client <sock> <noroot_sock> | client-bigreply <sock> | "
            "server <sock> <max_threads>\n",
            argv[0]);
    return 2;
}

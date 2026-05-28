// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Subplan 2-12 Phase D / **AC-12.6** STAGE3 — real-libbinder client
// for the **multi-connection-per-session** interop gate.
//
// Pairs with `example-hello/src/bin/rpc_multiconn_interop_server`
// (cross-compiled rsbinder server). The companion bash script
// `run_stage3_multiconn.sh` automates build + push + run.
//
// ⚠ **STATUS (2026-05-21): NOT YET PASSING — gate is blocked.** The
// harness compiles and runs end-to-end, but the (a) concurrent-twoway
// gate fails against the real android-16 libbinder peer: libbinder
// errors with `RpcState: Expecting 20 but got 0 bytes for RpcWireReply.
// Terminating!` after a single concurrent burst, killing the session
// for the rest of the run. A focused 2026-05-21 investigation hex-
// dumped rsbinder's server-side wire bytes (byte-correct on every
// send) and added a process-wide server-side send mutex (still fails)
// — wire-level interleaving is ruled out as the cause. Root cause is
// either a libbinder-side multi-outgoing-conn race we're triggering
// or a non-wire protocol nuance rsbinder Phase A misses; investigation
// is paused pending a deeper libbinder build/instrument session. This
// launcher stays in tree as the **future-AC-12.6 gate harness**:
// re-run `run_stage3_multiconn.sh` once the multi-conn root cause is
// known + fixed; the pass criteria below (a)/(b)/(c) and exit-codes
// stay as the contract.
//
// What this launcher verifies once the gate passes (Plan 2-12 §3
// AC-12.6 — *hermetic rsbinder↔rsbinder is byte-symmetric so
// F1/F5/F6-class defects only surface against the real peer*):
//
//   (a) **Concurrent twoway across N=2 outgoing slots**: two threads
//       each fire `TX_SLOW_ECHO(80ms)` in parallel; total wall ≤ 250ms
//       (sequential would be ≥160ms; this catches a silent
//       serialization on one slot — F2/F8-class regression signature).
//
//   (b) **Oneway in-order via founding-slot pin** (Plan 2-12 Option-1):
//       20 oneway `TX_ONEWAY(i)` then `TX_GET_LOG()` must return
//       `[0,1,…,19]` byte-exact (F5 reorder-buffer + F6 per-node
//       asyncNumber wire correctness).
//
//   (c) **Cross-slot nested callback**: register a callback `AIBinder`
//       (descriptor `rsbinder.test.IMultiConnCallback`), call
//       `TX_INVOKE_CALLBACK(cb, "ping")` twice in parallel on slots 0
//       and 1; server re-enters into the callback on the *same* slot
//       (DRIVING `(sess, slot)` re-entry pin); reply must equal
//       `cb-echo:ping`. Catches F8 (cross-slot proxy aliasing) +
//       AC-3.6 (nested callback no-deadlock under pool traversal).
//       Currently still requires F8.B (split mOutgoing/mIncoming
//       pools) — deferred behind AC-12.6 (a) passing first.
//
// Build (host):
//   $ANDROID_NDK_HOME/.../bin/aarch64-linux-android36-clang++ \
//       --target=aarch64-linux-android36 -O2 \
//       -L /tmp -lbinder_ndk -lbinder_rpc_unstable -llog \
//       rpc_multiconn_interop_launcher.cpp \
//       -o rpc_multiconn_interop_launcher

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <errno.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <unistd.h>

#include <atomic>
#include <chrono>
#include <string>
#include <thread>

extern "C" {
// NDK r29 sysroot only exposes the `libbinder_ndk` stub, so platform
// APIs need explicit `extern "C"` declarations cross-checked against
// AOSP android-16.0.0_r4 headers and linked at runtime from the
// device's libbinder_*.so (pulled to /tmp by the run script).

// --- libbinder_ndk.so ---
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

constexpr const char* kRootDescriptor = "rsbinder.test.IMultiConn";
// Reserved for the future (c) cross-slot nested callback gate (F8.B).
[[maybe_unused]] constexpr const char* kCallbackDescriptor =
        "rsbinder.test.IMultiConnCallback";

// Must match the rsbinder server's TX_* constants.
constexpr transaction_code_t TX_ECHO = FIRST_CALL_TRANSACTION + 0;
constexpr transaction_code_t TX_SLOW_ECHO = FIRST_CALL_TRANSACTION + 1;
constexpr transaction_code_t TX_ONEWAY = FIRST_CALL_TRANSACTION + 2;
constexpr transaction_code_t TX_GET_LOG = FIRST_CALL_TRANSACTION + 3;
constexpr transaction_code_t TX_INVOKE_CALLBACK = FIRST_CALL_TRANSACTION + 4;
constexpr transaction_code_t TX_CALLBACK_ECHO = FIRST_CALL_TRANSACTION + 0;

// --- AParcel string allocator (std::string sink) -----------------
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

// --- Connection provider -----------------------------------------
// `ARpcSession_setupPreconnectedClient` consumes the *initial* fd for
// the first connection, then calls `requestFd` for every subsequent
// connection (per AOSP `setupPreconnectedClient` lambda in
// `RpcSession.cpp:189-204`). So one provider can serve N outgoing +
// M incoming via N+M `connect(2)`s to the same UDS path.
struct ConnProvider {
    std::string sock_path;
    std::atomic<int> call_count{0};
};

int request_fd(void* param) {
    auto* p = static_cast<ConnProvider*>(param);
    int n = p->call_count.fetch_add(1, std::memory_order_acq_rel);
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd < 0) {
        perror("[cpp-client] request_fd socket");
        return -1;
    }
    sockaddr_un addr{};
    addr.sun_family = AF_UNIX;
    if (p->sock_path.size() >= sizeof(addr.sun_path)) {
        fprintf(stderr, "[cpp-client] sock path too long\n");
        close(fd);
        return -1;
    }
    strncpy(addr.sun_path, p->sock_path.c_str(), sizeof(addr.sun_path) - 1);
    socklen_t len = (socklen_t)(offsetof(sockaddr_un, sun_path) + p->sock_path.size() + 1);
    if (connect(fd, reinterpret_cast<sockaddr*>(&addr), len) < 0) {
        perror("[cpp-client] request_fd connect");
        close(fd);
        return -1;
    }
    fprintf(stderr, "[cpp-client] request_fd call#%d -> fd=%d (%s)\n", n, fd, p->sock_path.c_str());
    return fd;
}

void delete_provider(void* /*param*/) {
    // Storage is stack-allocated in main(); nothing to free.
}

// --- AIBinder ownership helpers (RAII) ---------------------------
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

// --- Callback class (server→client nested call target) -----------
// The rsbinder server's `TX_INVOKE_CALLBACK` reads a SIBinder + String,
// then calls `cb.transact(TX_CALLBACK_ECHO, &d, 0)` with the bare
// `writeString16(descriptor)` interface token (2-8 STAGE3 RPC token
// convention). The NDK dispatches that to our `on_transact` here.
binder_status_t callback_on_transact(AIBinder* /*binder*/, transaction_code_t code,
                                     const AParcel* in, AParcel* out) {
    if (code != TX_CALLBACK_ECHO) {
        return STATUS_UNKNOWN_TRANSACTION;
    }
    std::string s;
    binder_status_t rc = AParcel_readString(in, &s, read_string_into);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-cb] readString: %d\n", rc);
        return rc;
    }
    if (!s.empty() && s.back() == '\0') s.pop_back();
    std::string reply = "cb-echo:" + s;
    fprintf(stderr, "[cpp-cb] on_transact arg=%.*s reply=%.*s\n",
            (int)s.size(), s.data(), (int)reply.size(), reply.data());
    AStatusOwned st_ok{.s = AStatus_newOk()};
    rc = AParcel_writeStatusHeader(out, st_ok.s);
    if (rc != STATUS_OK) return rc;
    return AParcel_writeString(out, reply.c_str(), (int32_t)reply.size());
}

void* cb_on_create(void* args) { return args; }
void cb_on_destroy(void* /*userData*/) {}

// --- Twoway / Oneway helpers --------------------------------------
bool do_echo(AIBinder* root, const char* arg, std::string* out_reply) {
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] prep(echo): %d\n", rc);
        return false;
    }
    rc = AParcel_writeString(in.p, arg, (int32_t)strlen(arg));
    if (rc != STATUS_OK) return false;
    OutParcel out;
    rc = AIBinder_transact(root, TX_ECHO, &in.p, &out.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(echo): %d\n", rc);
        return false;
    }
    AStatusOwned st;
    rc = AParcel_readStatusHeader(out.p, &st.s);
    if (rc != STATUS_OK || !AStatus_isOk(st.s)) {
        fprintf(stderr, "[cpp-client] echo non-OK Status\n");
        return false;
    }
    rc = AParcel_readString(out.p, out_reply, read_string_into);
    if (rc != STATUS_OK) return false;
    if (!out_reply->empty() && out_reply->back() == '\0') out_reply->pop_back();
    return true;
}

bool do_slow_echo(AIBinder* root, const char* arg, int32_t ms, std::string* out_reply) {
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) return false;
    rc = AParcel_writeString(in.p, arg, (int32_t)strlen(arg));
    if (rc != STATUS_OK) return false;
    rc = AParcel_writeInt32(in.p, ms);
    if (rc != STATUS_OK) return false;
    OutParcel out;
    rc = AIBinder_transact(root, TX_SLOW_ECHO, &in.p, &out.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(slow_echo): %d\n", rc);
        return false;
    }
    AStatusOwned st;
    rc = AParcel_readStatusHeader(out.p, &st.s);
    if (rc != STATUS_OK || !AStatus_isOk(st.s)) return false;
    rc = AParcel_readString(out.p, out_reply, read_string_into);
    if (rc != STATUS_OK) return false;
    if (!out_reply->empty() && out_reply->back() == '\0') out_reply->pop_back();
    return true;
}

bool do_oneway(AIBinder* root, int32_t i) {
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) return false;
    rc = AParcel_writeInt32(in.p, i);
    if (rc != STATUS_OK) return false;
    OutParcel out;
    rc = AIBinder_transact(root, TX_ONEWAY, &in.p, &out.p, FLAG_ONEWAY);
    in.p = nullptr;
    return rc == STATUS_OK;
}

bool do_get_log(AIBinder* root, std::vector<int32_t>* out_log) {
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) return false;
    OutParcel out;
    rc = AIBinder_transact(root, TX_GET_LOG, &in.p, &out.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(get_log): %d\n", rc);
        return false;
    }
    AStatusOwned st;
    rc = AParcel_readStatusHeader(out.p, &st.s);
    if (rc != STATUS_OK || !AStatus_isOk(st.s)) return false;
    // Vec<i32> wire: int32 length + length × int32. Matches rsbinder's
    // `Parcel::write(&Vec<i32>)` and AIDL int32 array convention.
    int32_t len = 0;
    rc = AParcel_readInt32(out.p, &len);
    if (rc != STATUS_OK) return false;
    if (len < 0) {
        fprintf(stderr, "[cpp-client] get_log got null vec\n");
        return false;
    }
    out_log->resize((size_t)len);
    for (int32_t i = 0; i < len; ++i) {
        rc = AParcel_readInt32(out.p, &(*out_log)[i]);
        if (rc != STATUS_OK) return false;
    }
    return true;
}

// Reserved for the future (c) cross-slot nested callback gate (F8.B).
[[maybe_unused]]
bool do_invoke_callback(AIBinder* root, AIBinder* cb, const char* arg,
                         std::string* out_reply) {
    InParcel in;
    binder_status_t rc = AIBinder_prepareTransaction(root, &in.p);
    if (rc != STATUS_OK) return false;
    rc = AParcel_writeStrongBinder(in.p, cb);
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] writeStrongBinder: %d\n", rc);
        return false;
    }
    rc = AParcel_writeString(in.p, arg, (int32_t)strlen(arg));
    if (rc != STATUS_OK) return false;
    OutParcel out;
    rc = AIBinder_transact(root, TX_INVOKE_CALLBACK, &in.p, &out.p, 0);
    in.p = nullptr;
    if (rc != STATUS_OK) {
        fprintf(stderr, "[cpp-client] transact(invoke_callback): %d\n", rc);
        return false;
    }
    AStatusOwned st;
    rc = AParcel_readStatusHeader(out.p, &st.s);
    if (rc != STATUS_OK || !AStatus_isOk(st.s)) {
        fprintf(stderr, "[cpp-client] invoke_callback non-OK Status\n");
        return false;
    }
    rc = AParcel_readString(out.p, out_reply, read_string_into);
    if (rc != STATUS_OK) return false;
    if (!out_reply->empty() && out_reply->back() == '\0') out_reply->pop_back();
    return true;
}

} // namespace

int main(int argc, char** argv) {
    const char* sock_path = (argc > 1) ? argv[1] : "/data/local/tmp/rsmc.sock";
    fprintf(stderr, "[cpp-client] AC-12.6 STAGE3 multi-conn sock=%s\n", sock_path);

    // 0) Kernel-binder thread pool — required because incoming server→
    //    client callbacks are dispatched on the libbinder thread pool.
    //    Without it, our `callback_on_transact` would never run.
    ABinderProcess_setThreadPoolMaxThreadCount(4);
    ABinderProcess_startThreadPool();

    // 1) Build the RPC session with N=2 outgoing connections + N=2
    //    incoming threads. The launcher picks `requestFd` strategy
    //    (open a fresh `connect(2)` on every call), so the session
    //    machinery negotiates N=2 outgoing + N=2 incoming = 4 fds.
    ARpcSession* session_raw = ARpcSession_new();
    // M16 RAII (review 2026-05-21).
    auto session_owner = std::unique_ptr<ARpcSession, decltype(&ARpcSession_free)>(
            session_raw, ARpcSession_free);
    ARpcSession* session = session_owner.get();
    ARpcSession_setMaxOutgoingConnections(session, 2);
    // 1 incoming covers (c) callback once F8.B lands; (a)/(b) need only
    // outgoing. The 2026-05-21 investigation found that even with
    // `setMaxIncomingThreads(0)` (outgoing-only) the (a) gate fails the
    // same way, so this value isn't load-bearing for the current
    // unresolved failure mode.
    ARpcSession_setMaxIncomingThreads(session, 1);

    ConnProvider provider{.sock_path = sock_path};
    AIBinder* root_raw =
            ARpcSession_setupPreconnectedClient(session, request_fd, &provider, delete_provider);
    if (!root_raw) {
        fprintf(stderr, "[cpp-client] setupPreconnectedClient returned null\n");
        return 1;
    }
    auto root_owner = std::unique_ptr<AIBinder, decltype(&AIBinder_decStrong)>(
            root_raw, AIBinder_decStrong);
    AIBinder* root = root_owner.get();
    fprintf(stderr, "[cpp-client] root acquired; total request_fd calls=%d\n",
            provider.call_count.load());

    AIBinder_Class* root_clazz = AIBinder_Class_define(
            kRootDescriptor, cb_on_create, cb_on_destroy, callback_on_transact);
    if (!root_clazz) {
        fprintf(stderr, "[cpp-client] Class_define(root) failed\n");
        return 2;
    }
    if (!AIBinder_associateClass(root, root_clazz)) {
        fprintf(stderr, "[cpp-client] associateClass(root) failed\n");
        return 3;
    }

    // Sanity: a plain echo round-trip works before we exercise the
    // multi-conn-specific gates.
    {
        std::string r;
        if (!do_echo(root, "sanity", &r) || r != "sanity") {
            fprintf(stderr, "[cpp-client] sanity echo failed: got=%.*s\n",
                    (int)r.size(), r.data());
            return 4;
        }
    }
    fprintf(stderr, "[cpp-client] sanity OK\n");

    // ---- (a) Concurrent twoway across N=2 slots --------------------
    // Two threads, each calling TX_SLOW_ECHO(80ms). Parallel ≤ ~250ms;
    // serialized would land near 160ms+IPC, but jitter on the
    // emulator can push a *parallel* run past 150ms without indicating
    // a real serialization regression. The 250ms wall budget matches
    // the upstream design doc (server-side rustdoc + plan 2-12) and
    // is still tight enough to fail a "single-slot serialized"
    // fallback (which would land closer to 200ms in practice once
    // emulator scheduling overhead is added). M17 fix (review
    // 2026-05-21).
    {
        fprintf(stderr, "[cpp-client] (a) concurrent twoway: 2 threads × TX_SLOW_ECHO(80ms)\n");
        std::string r1, r2;
        bool ok1 = false, ok2 = false;
        auto start = std::chrono::steady_clock::now();
        std::thread t1([&]() { ok1 = do_slow_echo(root, "thread-1", 80, &r1); });
        std::thread t2([&]() { ok2 = do_slow_echo(root, "thread-2", 80, &r2); });
        t1.join();
        t2.join();
        auto elapsed_ms = std::chrono::duration_cast<std::chrono::milliseconds>(
                                  std::chrono::steady_clock::now() - start)
                                  .count();
        fprintf(stderr,
                "[cpp-client] (a) elapsed=%lld ms (ok1=%d r1=%s, ok2=%d r2=%s)\n",
                (long long)elapsed_ms, ok1, r1.c_str(), ok2, r2.c_str());
        if (!ok1 || !ok2 || r1 != "thread-1" || r2 != "thread-2") {
            fprintf(stderr, "[cpp-client] (a) reply mismatch\n");
            return 10;
        }
        if (elapsed_ms > 250) {
            fprintf(stderr,
                    "[cpp-client] (a) FAIL: elapsed %lldms > 250ms — likely serialized "
                    "on a single slot (multi-conn regression)\n",
                    (long long)elapsed_ms);
            return 11;
        }
        fprintf(stderr, "[cpp-client] (a) PASS — parallel within budget\n");
    }

    // ---- (b) Oneway in-order via Phase C per-node asyncTodo ---------
    // 20 oneway calls then polled twoway TX_GET_LOG (drain wait). The
    // Phase C `asyncTodo` priority replay (AOSP `RpcState::process
    // TransactInternal` lines 1093–1133 enqueue + 1247–1278 drain)
    // guarantees per-node monotonic order *eventually*: libbinder's
    // `ExclusiveConnection::find(CLIENT_ASYNC)` round-robins oneway
    // across the client's `mOutgoing` pool, so the rsbinder server
    // receives them split across slots whose `serve_blocking_on`
    // workers dispatch independently. With Phase C the server parks
    // out-of-order arrivals in the target node's `asyncTodo` heap and
    // drains them when the matching expected `async_number` arrives.
    //
    // The crucial wrinkle: **TX_GET_LOG is a twoway, not gated by
    // asyncTodo**, so it can race a still-draining queue. The test
    // poll-loops `do_get_log` until `log.size() == kN` or a 2 s
    // timeout, then asserts strict order — the in-order guarantee is
    // structural (per-node monotonic), only the *timing* needs the
    // poll to bound the eventual-consistency window. Pre-Phase-C this
    // test got `log size 10` (founding slot's drain) or `log size 1`
    // (asyncTodo with no drain wait); Phase C + poll gets `log size
    // 20` in strict order.
    {
        constexpr int kN = 20;
        fprintf(stderr, "[cpp-client] (b) oneway in-order: %d × TX_ONEWAY then TX_GET_LOG\n", kN);
        for (int i = 0; i < kN; ++i) {
            if (!do_oneway(root, i)) {
                fprintf(stderr, "[cpp-client] (b) oneway(%d) send failed\n", i);
                return 20;
            }
        }
        std::vector<int32_t> log;
        constexpr int kTimeoutMs = 2000;
        constexpr int kPollMs = 25;
        int elapsed_ms = 0;
        while (elapsed_ms <= kTimeoutMs) {
            log.clear();
            if (!do_get_log(root, &log)) {
                fprintf(stderr, "[cpp-client] (b) get_log failed\n");
                return 21;
            }
            if ((int)log.size() == kN) break;
            usleep(kPollMs * 1000);
            elapsed_ms += kPollMs;
        }
        if ((int)log.size() != kN) {
            fprintf(stderr,
                    "[cpp-client] (b) FAIL: log size %zu != %d after %d ms poll\n",
                    log.size(), kN, elapsed_ms);
            return 22;
        }
        for (int i = 0; i < kN; ++i) {
            if (log[i] != i) {
                fprintf(stderr,
                        "[cpp-client] (b) FAIL: log[%d]=%d, expected %d — oneway reorder \
(Phase C asyncTodo bug)\n",
                        i, log[i], i);
                return 23;
            }
        }
        fprintf(stderr,
                "[cpp-client] (b) PASS — %d oneway calls in-order (drained after %d ms)\n",
                kN, elapsed_ms);
    }

    // ---- (c) Cross-slot nested callback (AC-12.2-extended) ----------
    // 2 client threads fire TX_INVOKE_CALLBACK(cb, "pingN") in parallel
    // on the 2 outgoing slots. The rsbinder server dispatches each on
    // its own incoming-slot worker; inside on_transact, the server
    // makes a *nested* server→client transact `cb.transact(TX_CALLBACK_
    // ECHO, "pingN")` and reads the reply. The nested send rides the
    // **same slot** the original transact arrived on via the Phase A
    // `find_conn` DRIVING `(sess, slot)` re-entry pin — bidirectional
    // wire on one TCP socket. The libbinder client's `waitForReply`
    // loop on that slot accepts an inbound TRANSACT (nested context),
    // dispatches our `callback_on_transact`, writes the reply back on
    // the same slot, and returns to its outer wait. The launcher
    // asserts both threads receive `cb-echo:pingN` (no cross-worker
    // wire interleave / aliasing — F8). Pre-Phase-A4 (server-side
    // N-inner) shared `state.remote_proxies` across workers, so the
    // 2nd worker's nested send could route through the *first*
    // worker's inner socket and deadlock or interleave; A4 + DRIVING
    // (sess,slot) key fixed that, and this gate proves the integration.
    {
        constexpr int kN = 2;
        fprintf(stderr,
                "[cpp-client] (c) cross-slot nested callback: %d × TX_INVOKE_CALLBACK in parallel\n",
                kN);
        AIBinder_Class* cb_clazz = AIBinder_Class_define(
                kCallbackDescriptor, cb_on_create, cb_on_destroy, callback_on_transact);
        if (!cb_clazz) {
            fprintf(stderr, "[cpp-client] (c) Class_define(cb) failed\n");
            return 30;
        }
        AIBinder* cb_raw = AIBinder_new(cb_clazz, nullptr);
        if (!cb_raw) {
            fprintf(stderr, "[cpp-client] (c) AIBinder_new(cb) failed\n");
            return 31;
        }
        auto cb_owner = std::unique_ptr<AIBinder, decltype(&AIBinder_decStrong)>(
                cb_raw, AIBinder_decStrong);
        AIBinder* cb = cb_owner.get();

        std::vector<std::thread> ths;
        std::vector<std::string> replies(kN);
        std::vector<int> oks(kN, 0);
        for (int i = 0; i < kN; ++i) {
            ths.emplace_back([i, root, cb, &replies, &oks]() {
                std::string arg = "ping" + std::to_string(i);
                if (do_invoke_callback(root, cb, arg.c_str(), &replies[i])) {
                    oks[i] = 1;
                }
            });
        }
        for (auto& t : ths) t.join();
        for (int i = 0; i < kN; ++i) {
            std::string want = "cb-echo:ping" + std::to_string(i);
            if (!oks[i] || replies[i] != want) {
                fprintf(stderr,
                        "[cpp-client] (c) FAIL thread %d: ok=%d reply=\"%.*s\" want=\"%.*s\"\n",
                        i, oks[i], (int)replies[i].size(), replies[i].data(),
                        (int)want.size(), want.data());
                return 32;
            }
        }
        fprintf(stderr, "[cpp-client] (c) PASS — %d parallel nested callbacks round-tripped\n", kN);
    }

    printf("AC-12.6 PASS — multi-conn real-libbinder ↔ rsbinder full transact\n");
    fflush(stdout);
    // M16 RAII: `session_owner`/`root_owner` clean up on return.
    return 0;
}

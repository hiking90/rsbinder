// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 2-14 Phase C — real-libbinder client side of the VINTF-driven
// accessor routing harness. Unlike the 2-13 D.8 launcher (which had to
// manually `BpAccessor::from_binder` + `addConnection` because the
// stock emulator lacked VINTF), this client trusts servicemanager to
// route through the `Service::Accessor(Some(_))` arm thanks to the
// `<accessor>` entry pushed to `/system_ext/etc/vintf/manifest/`
// (`rsbinder_phasec_accessor.xml`).
//
// Flow:
//   1. `AServiceManager_waitForService("rsbinder.test.accessor.IInterop/default")`
//      — BackendUnifiedServiceManager (libbinder_ndk) consults
//      servicemanager which consults VINTF, locates the
//      `<accessor>android.os.IAccessor/IInterop/default</accessor>`
//      tag, looks up that IAccessor binder, and silently invokes
//      `addConnection()` to set up the RPC bridge. The AIBinder we
//      get back is *already* the RPC root.
//   2. Interface-associate with the rsbinder server's root descriptor
//      `"rsbinder.test.accessor.IInterop"` so AIDL token-style
//      transactions line up with the server's `on_transact`.
//   3. Transact TX_ECHO + TX_GIVE_MARKER, assert byte-equal replies.
//
// Exit code 0 = PASS (VINTF + Accessor arm + RPC bridge all working);
// non-zero = FAIL with stderr diagnostic.
//
// Build (cross, NDK):
//   $ANDROID_NDK_HOME/.../bin/aarch64-linux-androidNN-clang++ \
//       --target=aarch64-linux-androidNN -O2 -static-libstdc++ \
//       -lbinder_ndk -llog \
//       phasec_vintf_client.cpp -o phasec_vintf_client
//
// Note: this client does *not* link libbinder_rpc_unstable; the RPC
// session is set up inside libbinder_ndk by BackendUnifiedServiceManager
// during AServiceManager_waitForService, so the caller never touches
// the RPC ABI directly. That's the whole point of Phase C — the AOSP-
// faithful path makes accessors look like regular services.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <cstdio>
#include <cstring>
#include <string>

// NDK r29 ships only the three binder headers above (`binder_parcel.h`,
// `binder_ibinder.h`, `binder_status.h`) — `binder_manager.h` and the
// `AServiceManager_*` C entry points are platform-only headers, not
// exposed in the public NDK sysroot. The symbols themselves are
// available at runtime through `libbinder_ndk.so`, which Android device
// images always ship in `/system/lib64/`. Same trick the 2-13 STAGE3
// launcher uses; we declare the prototype `extern "C"` so the linker
// resolves it against the device-side shared object at load time.
extern "C" AIBinder* AServiceManager_waitForService(const char* instance);

// Must match the rsbinder server's `ROOT_DESC` constant (see
// `example-hello/src/bin/rpc_accessor_register_interop_server.rs`).
static constexpr const char* kRootDesc = "rsbinder.test.accessor.IInterop";
// Service name we ask for. The package + interface portion must match
// the `<name>` + `<fqname>` of `rsbinder_phasec_accessor.xml`; trailing
// `/default` is the instance.
static constexpr const char* kServiceName =
    "rsbinder.test.accessor.IInterop/default";
// Server-side hardcoded marker (mirrors the server's `MARKER` const).
static constexpr const char* kExpectedMarker = "stage3-from-rsbinder";

static constexpr transaction_code_t kTxEcho = FIRST_CALL_TRANSACTION;
static constexpr transaction_code_t kTxGiveMarker = FIRST_CALL_TRANSACTION + 1;

// IAccessor doesn't itself surface here — we trust libbinder_ndk to
// bridge transparently. The `AIBinder_Class` is just so we can
// `associateClass` and use prepareTransaction/transact below.
static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}
static binder_status_t on_transact(AIBinder*, transaction_code_t, const AParcel*,
                                   AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

int main() {
    fprintf(stderr, "[phasec-client] requesting %s via servicemanager\n",
            kServiceName);

    AIBinder_Class* clazz =
        AIBinder_Class_define(kRootDesc, on_create, on_destroy, on_transact);
    if (!clazz) {
        fprintf(stderr, "AIBinder_Class_define failed\n");
        return 2;
    }

    AIBinder* root = AServiceManager_waitForService(kServiceName);
    if (!root) {
        fprintf(stderr,
                "[phasec-client] FAIL: AServiceManager_waitForService(%s) "
                "returned null — VINTF accessor entry not loaded, accessor "
                "binder not registered, or BackendUnifiedServiceManager "
                "didn't route through the Accessor arm.\n",
                kServiceName);
        return 3;
    }
    fprintf(stderr, "[phasec-client] AIBinder acquired — VINTF arm worked\n");

    if (!AIBinder_associateClass(root, clazz)) {
        fprintf(stderr,
                "AIBinder_associateClass failed — expected descriptor %s "
                "did not match the binder\n",
                kRootDesc);
        AIBinder_decStrong(root);
        return 4;
    }

    // TX_GIVE_MARKER — no input parcel body beyond the AIDL header.
    {
        AParcel* in = nullptr;
        AParcel* out = nullptr;
        binder_status_t st =
            AIBinder_prepareTransaction(root, &in);
        if (st != STATUS_OK) {
            fprintf(stderr, "prepareTransaction failed: %d\n", st);
            AIBinder_decStrong(root);
            return 5;
        }
        st = AIBinder_transact(root, kTxGiveMarker, &in, &out, 0);
        if (st != STATUS_OK) {
            fprintf(stderr, "TX_GIVE_MARKER transact failed: %d\n", st);
            AIBinder_decStrong(root);
            return 6;
        }
        // `AParcel_readStatusHeader` requires a non-null AStatus**.
        // The server emits `Status::Ok` (= writeInt32(0)), so the
        // returned AStatus is `STATUS_OK` and we delete it immediately.
        AStatus* astatus = nullptr;
        st = AParcel_readStatusHeader(out, &astatus);
        if (st != STATUS_OK) {
            fprintf(stderr, "readStatusHeader failed: %d\n", st);
            AParcel_delete(out);
            AIBinder_decStrong(root);
            return 7;
        }
        if (!AStatus_isOk(astatus)) {
            fprintf(stderr, "Status non-OK: %d\n",
                    AStatus_getStatus(astatus));
            AStatus_delete(astatus);
            AParcel_delete(out);
            AIBinder_decStrong(root);
            return 7;
        }
        AStatus_delete(astatus);
        struct Buf {
            std::string s;
        } buf;
        auto allocator = [](void* userdata, int32_t len, char** out) -> bool {
            if (len <= 0) {
                *out = nullptr;
                return true;
            }
            auto* b = static_cast<Buf*>(userdata);
            b->s.resize(len - 1);  // len includes null terminator in NDK
            *out = b->s.data();
            return true;
        };
        st = AParcel_readString(out, &buf, allocator);
        AParcel_delete(out);
        if (st != STATUS_OK) {
            fprintf(stderr, "readString(marker) failed: %d\n", st);
            AIBinder_decStrong(root);
            return 8;
        }
        if (buf.s != kExpectedMarker) {
            fprintf(stderr,
                    "[phasec-client] FAIL: marker mismatch — got %s, expected %s\n",
                    buf.s.c_str(), kExpectedMarker);
            AIBinder_decStrong(root);
            return 9;
        }
        fprintf(stderr, "[phasec-client] TX_GIVE_MARKER OK: %s\n",
                buf.s.c_str());
    }

    // TX_ECHO — write a String and verify the echo round-trip.
    {
        AParcel* in = nullptr;
        AParcel* out = nullptr;
        binder_status_t st =
            AIBinder_prepareTransaction(root, &in);
        if (st != STATUS_OK) {
            fprintf(stderr, "prepareTransaction (echo) failed: %d\n", st);
            AIBinder_decStrong(root);
            return 10;
        }
        const char* echo_in = "hello-phasec";
        st = AParcel_writeString(in, echo_in, strlen(echo_in));
        if (st != STATUS_OK) {
            fprintf(stderr, "writeString failed: %d\n", st);
            AParcel_delete(in);
            AIBinder_decStrong(root);
            return 11;
        }
        st = AIBinder_transact(root, kTxEcho, &in, &out, 0);
        if (st != STATUS_OK) {
            fprintf(stderr, "TX_ECHO transact failed: %d\n", st);
            AIBinder_decStrong(root);
            return 12;
        }
        AStatus* astatus = nullptr;
        st = AParcel_readStatusHeader(out, &astatus);
        if (st != STATUS_OK) {
            fprintf(stderr, "readStatusHeader (echo) failed: %d\n", st);
            AParcel_delete(out);
            AIBinder_decStrong(root);
            return 13;
        }
        if (!AStatus_isOk(astatus)) {
            fprintf(stderr, "Status (echo) non-OK: %d\n",
                    AStatus_getStatus(astatus));
            AStatus_delete(astatus);
            AParcel_delete(out);
            AIBinder_decStrong(root);
            return 13;
        }
        AStatus_delete(astatus);
        struct Buf {
            std::string s;
        } buf;
        auto allocator = [](void* userdata, int32_t len, char** out) -> bool {
            if (len <= 0) {
                *out = nullptr;
                return true;
            }
            auto* b = static_cast<Buf*>(userdata);
            b->s.resize(len - 1);
            *out = b->s.data();
            return true;
        };
        st = AParcel_readString(out, &buf, allocator);
        AParcel_delete(out);
        if (st != STATUS_OK) {
            fprintf(stderr, "readString(echo) failed: %d\n", st);
            AIBinder_decStrong(root);
            return 14;
        }
        if (buf.s != echo_in) {
            fprintf(stderr,
                    "[phasec-client] FAIL: echo mismatch — got %s, expected %s\n",
                    buf.s.c_str(), echo_in);
            AIBinder_decStrong(root);
            return 15;
        }
        fprintf(stderr, "[phasec-client] TX_ECHO OK: %s\n", buf.s.c_str());
    }

    AIBinder_decStrong(root);
    fprintf(stderr,
            "[phasec-client] PASS — VINTF accessor routing + RPC bridge + "
            "byte-correct round-trips both verified.\n");
    return 0;
}

// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Real-libbinder cross-check for the argument shapes whose generated Rust
// changed with the 2026-08 AIDL review. Client side; the server is
// `example-hello/src/bin/codegen_shapes_interop_service.rs`.
//
// Why a C++ peer at all: `tests/tests/codegen_shapes.rs` round-trips the
// same interface rsbinder-to-rsbinder, which passes for *any* self-
// consistent layout. Here every array goes through libbinder_ndk's own
// `AParcel_writeInt32Array` / `AParcel_readInt32Array`, so the layout under
// test is the one AOSP's runtime defines. A per-element null marker — what
// `@nullable int[]` used to emit — makes those helpers misparse.
//
// What is asserted:
//   1. `inout @nullable int[]`  — [1,2,3] comes back [1,2,3,99]; null stays null.
//   2. `in/out @nullable int[3]` — [4,5,6] comes back [6,5,4]; null stays null.
//   3. `out IBinder` (non-nullable) — filled, it round-trips; left unset, the
//      call must fail observably rather than deliver a null binder.
//
// NDK-only: no AOSP checkout, no headers or libraries pulled off the device.
// The three `AServiceManager_*` entry points are platform-only (libbinder_ndk
// exports them; the NDK sysroot ships no header), so they are resolved with
// `dlsym` rather than linked — a missing symbol is then a null pointer this
// program reports, not a load-time abort.
//
// Build + run: `./run_codegen_shapes_interop.sh [-s <device>]`.
// Exit 0 = PASS.

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <dlfcn.h>
#include <stdio.h>

#include <cstring>
#include <vector>

namespace {

constexpr const char* kDescriptor = "shapes.ICodegenShapes";
constexpr const char* kServiceName = "rsbinder.test.shapes";

// Method order in `ICodegenShapes.aidl`.
constexpr transaction_code_t kTxTakeOutBinder = FIRST_CALL_TRANSACTION;
constexpr transaction_code_t kTxRoundNullableVec = FIRST_CALL_TRANSACTION + 1;
constexpr transaction_code_t kTxRoundNullableFixed = FIRST_CALL_TRANSACTION + 2;

int failures = 0;

void check(bool ok, const char* what) {
    printf("%s %s\n", ok ? "  PASS" : "  FAIL", what);
    if (!ok) failures++;
}

// `AParcel_readInt32Array` calls this to size the destination. `length == -1`
// is the null array; anything else is the element count the peer wrote.
struct I32Array {
    std::vector<int32_t> data;
    bool is_null = false;
};

bool i32_allocator(void* arrayData, int32_t length, int32_t** outBuffer) {
    auto* dst = static_cast<I32Array*>(arrayData);
    if (length < 0) {
        dst->is_null = true;
        *outBuffer = nullptr;
        return true;
    }
    dst->is_null = false;
    dst->data.assign(static_cast<size_t>(length), 0);
    *outBuffer = dst->data.empty() ? nullptr : dst->data.data();
    return true;
}

// A transact whose reply we still need to read. Owns nothing on failure.
struct Reply {
    AParcel* parcel = nullptr;
    binder_status_t transact_status = STATUS_OK;
    int32_t exception = EX_NONE;
    bool ok() const { return transact_status == STATUS_OK && exception == EX_NONE; }
    ~Reply() {
        if (parcel) AParcel_delete(parcel);
    }
};

// Runs one transaction and consumes the AIDL status header, mirroring what a
// generated AOSP client does before touching any out argument.
bool transact(AIBinder* binder, transaction_code_t code,
              bool (*write_args)(AParcel*, void*), void* ctx, Reply* reply) {
    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st != STATUS_OK) {
        fprintf(stderr, "  prepareTransaction failed: %d\n", st);
        return false;
    }
    if (write_args && !write_args(in, ctx)) {
        AParcel_delete(in);
        return false;
    }
    reply->transact_status = AIBinder_transact(binder, code, &in, &reply->parcel, 0);
    if (reply->transact_status != STATUS_OK) {
        return true;  // A transact-level failure is itself an observation.
    }
    AStatus* status = nullptr;
    st = AParcel_readStatusHeader(reply->parcel, &status);
    if (st != STATUS_OK) {
        fprintf(stderr, "  readStatusHeader failed: %d\n", st);
        return false;
    }
    reply->exception = AStatus_getExceptionCode(status);
    AStatus_delete(status);
    return true;
}

struct VecArgs {
    const int32_t* data;
    int32_t length;
};

bool write_i32_array(AParcel* p, void* ctx) {
    auto* a = static_cast<VecArgs*>(ctx);
    binder_status_t st = AParcel_writeInt32Array(p, a->data, a->length);
    if (st != STATUS_OK) fprintf(stderr, "  writeInt32Array failed: %d\n", st);
    return st == STATUS_OK;
}

struct OutBinderArgs {
    AIBinder* src;
    bool fill;
};

bool write_out_binder_args(AParcel* p, void* ctx) {
    auto* a = static_cast<OutBinderArgs*>(ctx);
    if (AParcel_writeStrongBinder(p, a->src) != STATUS_OK) return false;
    return AParcel_writeBool(p, a->fill) == STATUS_OK;
}

// ---- the three cases ------------------------------------------------

void test_nullable_vec(AIBinder* binder) {
    printf("[1] inout @nullable int[]\n");
    {
        const int32_t sent[] = {1, 2, 3};
        VecArgs args{sent, 3};
        Reply reply;
        if (!transact(binder, kTxRoundNullableVec, write_i32_array, &args, &reply)) {
            check(false, "transact");
            return;
        }
        check(reply.ok(), "reply status is EX_NONE");
        if (!reply.ok()) return;
        I32Array got;
        binder_status_t st = AParcel_readInt32Array(reply.parcel, &got, i32_allocator);
        check(st == STATUS_OK, "libbinder_ndk parses the returned int32 array");
        if (st != STATUS_OK) {
            fprintf(stderr,
                    "  readInt32Array=%d — a per-element null marker would land here\n", st);
            return;
        }
        const std::vector<int32_t> want{1, 2, 3, 99};
        check(!got.is_null && got.data == want, "[1,2,3] -> [1,2,3,99]");
        if (got.data != want) {
            fprintf(stderr, "  got %zu elements:", got.data.size());
            for (int32_t v : got.data) fprintf(stderr, " %d", v);
            fprintf(stderr, "\n");
        }
    }
    {
        VecArgs args{nullptr, -1};
        Reply reply;
        if (!transact(binder, kTxRoundNullableVec, write_i32_array, &args, &reply)) {
            check(false, "transact (null)");
            return;
        }
        if (!reply.ok()) {
            check(false, "reply status is EX_NONE (null)");
            return;
        }
        I32Array got;
        binder_status_t st = AParcel_readInt32Array(reply.parcel, &got, i32_allocator);
        check(st == STATUS_OK && got.is_null, "null stays null");
    }
}

void test_nullable_fixed(AIBinder* binder) {
    printf("[2] in/out @nullable int[3]\n");
    {
        const int32_t sent[] = {4, 5, 6};
        VecArgs args{sent, 3};
        Reply reply;
        if (!transact(binder, kTxRoundNullableFixed, write_i32_array, &args, &reply)) {
            check(false, "transact");
            return;
        }
        check(reply.ok(), "reply status is EX_NONE");
        if (!reply.ok()) return;
        I32Array got;
        binder_status_t st = AParcel_readInt32Array(reply.parcel, &got, i32_allocator);
        check(st == STATUS_OK, "libbinder_ndk parses the returned fixed array");
        if (st != STATUS_OK) return;
        const std::vector<int32_t> want{6, 5, 4};
        check(!got.is_null && got.data == want, "[4,5,6] -> [6,5,4]");
    }
    {
        VecArgs args{nullptr, -1};
        Reply reply;
        if (!transact(binder, kTxRoundNullableFixed, write_i32_array, &args, &reply)) {
            check(false, "transact (null)");
            return;
        }
        if (!reply.ok()) {
            check(false, "reply status is EX_NONE (null)");
            return;
        }
        I32Array got;
        binder_status_t st = AParcel_readInt32Array(reply.parcel, &got, i32_allocator);
        check(st == STATUS_OK && got.is_null, "null stays null");
    }
}

void test_out_binder(AIBinder* binder) {
    printf("[3] out IBinder (non-nullable)\n");
    {
        OutBinderArgs args{binder, true};
        Reply reply;
        if (!transact(binder, kTxTakeOutBinder, write_out_binder_args, &args, &reply)) {
            check(false, "transact (filled)");
            return;
        }
        check(reply.ok(), "reply status is EX_NONE");
        if (!reply.ok()) return;
        AIBinder* got = nullptr;
        binder_status_t st = AParcel_readStrongBinder(reply.parcel, &got);
        check(st == STATUS_OK && got != nullptr, "a filled out-binder round-trips");
        if (got) AIBinder_decStrong(got);
    }
    {
        OutBinderArgs args{binder, false};
        Reply reply;
        if (!transact(binder, kTxTakeOutBinder, write_out_binder_args, &args, &reply)) {
            check(false, "transact (unset)");
            return;
        }
        // A conforming client must not end up holding a null binder from a
        // call it believes succeeded. Any of: transact error, non-EX_NONE
        // status, unreadable binder — is an observable failure.
        bool delivered_null = false;
        if (reply.ok()) {
            AIBinder* got = nullptr;
            if (AParcel_readStrongBinder(reply.parcel, &got) == STATUS_OK && got == nullptr) {
                delivered_null = true;
            }
            if (got) AIBinder_decStrong(got);
        }
        printf("    (transact=%d exception=%d)\n", reply.transact_status, reply.exception);
        check(!delivered_null, "an unset out-binder is not delivered as a silent null");
        // The generated server arm turns the unset `Option` into
        // UNEXPECTED_NULL, which reaches the client as the transact status —
        // the same value AOSP's own generated server produces.
        check(reply.transact_status == STATUS_UNEXPECTED_NULL,
              "the failure surfaces as STATUS_UNEXPECTED_NULL");
    }
}

// Writes `@nullable int[]` the way rsbinder used to: a length, then a
// NON_NULL_PARCELABLE_FLAG word before every element. libbinder never writes
// that for a primitive element, so the two layouts must not be interchangeable
// — if the server accepted this one identically, cases [1] and [2] would be
// asserting nothing.
bool write_legacy_marked_array(AParcel* p, void* ctx) {
    auto* a = static_cast<VecArgs*>(ctx);
    if (AParcel_writeInt32(p, a->length) != STATUS_OK) return false;
    for (int32_t i = 0; i < a->length; i++) {
        if (AParcel_writeInt32(p, 1) != STATUS_OK) return false;  // not-null marker
        if (AParcel_writeInt32(p, a->data[i]) != STATUS_OK) return false;
    }
    return true;
}

void test_legacy_layout_is_not_accepted(AIBinder* binder) {
    printf("[4] the pre-review per-element-marker layout is a different wire\n");
    const int32_t sent[] = {1, 2, 3};
    VecArgs args{sent, 3};
    Reply reply;
    if (!transact(binder, kTxRoundNullableVec, write_legacy_marked_array, &args, &reply)) {
        check(false, "transact");
        return;
    }
    bool same_as_aosp_layout = false;
    binder_status_t read_st = STATUS_OK;
    if (reply.ok()) {
        I32Array got;
        read_st = AParcel_readInt32Array(reply.parcel, &got, i32_allocator);
        if (read_st == STATUS_OK) {
            const std::vector<int32_t> aosp{1, 2, 3, 99};
            same_as_aosp_layout = !got.is_null && got.data == aosp;
            printf("    decoded:");
            for (int32_t v : got.data) printf(" %d", v);
            printf("%s\n", got.is_null ? " (null)" : "");
        }
    }
    printf("    (transact=%d exception=%d read=%d)\n", reply.transact_status,
           reply.exception, read_st);
    check(!same_as_aosp_layout,
          "the legacy layout does not decode to the same values as the AOSP one");
}

// This process serves nothing; the class exists only so `associateClass`
// can match the descriptor and `prepareTransaction` write the AIDL header.
void* on_create(void*) { return nullptr; }
void on_destroy(void*) {}
binder_status_t on_transact(AIBinder*, transaction_code_t, const AParcel*, AParcel*) {
    return STATUS_UNKNOWN_TRANSACTION;
}

}  // namespace

int main() {
    void* lib = dlopen("libbinder_ndk.so", RTLD_NOW);
    if (!lib) {
        fprintf(stderr, "dlopen(libbinder_ndk.so) failed: %s\n", dlerror());
        return 2;
    }
    auto get_service = reinterpret_cast<AIBinder* (*)(const char*)>(
        dlsym(lib, "AServiceManager_waitForService"));
    if (!get_service) {
        fprintf(stderr, "dlsym(AServiceManager_waitForService) failed\n");
        return 2;
    }

    AIBinder_Class* clazz =
        AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    if (!clazz) {
        fprintf(stderr, "AIBinder_Class_define failed\n");
        return 2;
    }

    AIBinder* binder = get_service(kServiceName);
    if (!binder) {
        fprintf(stderr, "waitForService(%s) returned null\n", kServiceName);
        return 3;
    }
    if (!AIBinder_associateClass(binder, clazz)) {
        fprintf(stderr, "associateClass failed — descriptor is not %s\n", kDescriptor);
        AIBinder_decStrong(binder);
        return 4;
    }

    test_nullable_vec(binder);
    test_nullable_fixed(binder);
    test_out_binder(binder);
    test_legacy_layout_is_not_accepted(binder);

    AIBinder_decStrong(binder);
    if (failures == 0) {
        printf("CODEGEN_SHAPES_INTEROP_PASS\n");
        return 0;
    }
    printf("CODEGEN_SHAPES_INTEROP_FAIL (%d)\n", failures);
    return 1;
}

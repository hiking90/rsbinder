// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0
//
// Plan 10-2 AC-2.2 STAGE3: the blob wire between rsbinder and a real
// AOSP binder peer, both directions.
//
// **Scope, and why it stops where it does.** A blob is `int32 length`,
// `int32 tag`, then either inline bytes padded to 4 or a file-descriptor
// object for a shared region. This harness covers the inline form in
// both directions and, for the shared form, checks the tag rsbinder
// chose — it cannot produce or consume the fd form, because the NDK's
// AParcel has no bare file-descriptor API: `AParcel_writeParcelFileDescriptor`
// writes AIDL's `@nullable ParcelFileDescriptor` (an `int32 1` marker and
// then the object), while AOSP's `Parcel::writeBlob` writes the object
// alone. Only the platform C++ `Parcel` class exposes that, and it needs
// AOSP headers rather than the NDK.
//
// The fd form is therefore verified by two other means: libbinder's
// reader accepts a memfd (`ashmem_valid()` in libcutils reads
// /proc/self/fd/N and takes a `/memfd:` link, and sizes it with fstat),
// and the bare fd object rsbinder writes is already proven against real
// libbinder by the `IMemory` harness (`run_imemory_interop.sh`, plan
// 4-7a). Closing the remaining gap needs a box with an AOSP checkout.
//
//   blob_interop client <service-name> <bytes>
//   blob_interop server <service-name>
//
// Shares its contract with `tests/src/bin/blob_probe.rs`: descriptor
// "rsbinder.test.blob.IProbe", code 1 = GET (request: int32 length;
// reply: a blob of that length, payload `i % 251`).

#include <android/binder_ibinder.h>
#include <android/binder_parcel.h>
#include <android/binder_status.h>

#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <vector>

extern "C" {
AIBinder* AServiceManager_checkService(const char* instance);
binder_status_t AServiceManager_addService(AIBinder* binder, const char* instance);
void ABinderProcess_startThreadPool(void);
void ABinderProcess_joinThreadPool(void);
}

static constexpr const char* kDescriptor = "rsbinder.test.blob.IProbe";
static constexpr transaction_code_t kGet = FIRST_CALL_TRANSACTION;

// AOSP `Parcel.cpp` blob tags and limit.
static constexpr int32_t kBlobInplace = 0;
static constexpr int32_t kBlobAshmemImmutable = 1;
static constexpr int32_t kBlobAshmemMutable = 2;

static std::vector<uint8_t> pattern(size_t len) {
    std::vector<uint8_t> out(len);
    for (size_t i = 0; i < len; i++) out[i] = static_cast<uint8_t>(i % 251);
    return out;
}

static uint32_t checksum(const uint8_t* bytes, size_t len) {
    uint32_t acc = 0;
    for (size_t i = 0; i < len; i++)
        acc += static_cast<uint32_t>(i) ^ static_cast<uint32_t>(bytes[i]);
    return acc;
}

static void* on_create(void*) { return nullptr; }
static void on_destroy(void*) {}

static void put_i32(std::vector<uint8_t>& buf, int32_t v) {
    uint32_t u = static_cast<uint32_t>(v);
    for (int i = 0; i < 4; i++) buf.push_back(static_cast<uint8_t>(u >> (8 * i)));
}

static int32_t get_i32(const uint8_t* p) {
    uint32_t u = 0;
    for (int i = 0; i < 4; i++) u |= static_cast<uint32_t>(p[i]) << (8 * i);
    return static_cast<int32_t>(u);
}

// Inline blob only — see the scope note at the top.
//
// Assembled as raw bytes and moved in with `AParcel_unmarshal`, not
// written field by field: an inline payload is *packed* bytes padded to
// 4, and `AParcel_writeByte` is AIDL's `byte`, which occupies a whole
// int32 each. Writing the payload with it produces four times the bytes
// at the wrong offsets — measured, and the reason this went through a
// buffer instead.
static binder_status_t write_inline_blob(AParcel* out, const std::vector<uint8_t>& data) {
    std::vector<uint8_t> buf;
    put_i32(buf, static_cast<int32_t>(data.size()));
    put_i32(buf, kBlobInplace);
    buf.insert(buf.end(), data.begin(), data.end());
    buf.resize((buf.size() + 3) & ~size_t(3), 0);  // pad to the 4-byte grain
    return AParcel_unmarshal(out, buf.data(), buf.size());
}

// --- server role: answer with an inline blob -------------------------

static binder_status_t on_transact(AIBinder*, transaction_code_t code, const AParcel* in,
                                   AParcel* out) {
    if (code != kGet) return STATUS_UNKNOWN_TRANSACTION;
    int32_t len = 0;
    binder_status_t st = AParcel_readInt32(in, &len);
    if (st != STATUS_OK || len < 0) return STATUS_BAD_VALUE;
    fprintf(stderr, "[blob_interop] writing a %d-byte inline blob\n", len);
    return write_inline_blob(out, pattern(static_cast<size_t>(len)));
}

static int run_server(const char* name) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    AIBinder* binder = AIBinder_new(clazz, nullptr);
    if (!binder) {
        fprintf(stderr, "AIBinder_new failed\n");
        return 2;
    }
    binder_status_t st = AServiceManager_addService(binder, name);
    if (st != STATUS_OK) {
        fprintf(stderr, "AServiceManager_addService(%s) = %d\n", name, st);
        return 3;
    }
    printf("SERVING %s\n", name);
    fflush(stdout);
    ABinderProcess_startThreadPool();
    ABinderProcess_joinThreadPool();
    return 0;
}

// --- client role: read what rsbinder wrote ---------------------------

static int run_client(const char* name, size_t len) {
    AIBinder_Class* clazz = AIBinder_Class_define(kDescriptor, on_create, on_destroy, on_transact);
    AIBinder* binder = AServiceManager_checkService(name);
    if (!binder) {
        printf("RESULT cpp %zu ERROR not-registered\n", len);
        return 1;
    }
    if (!AIBinder_associateClass(binder, clazz)) {
        printf("RESULT cpp %zu ERROR descriptor-mismatch\n", len);
        AIBinder_decStrong(binder);
        return 1;
    }

    AParcel* in = nullptr;
    binder_status_t st = AIBinder_prepareTransaction(binder, &in);
    if (st == STATUS_OK) st = AParcel_writeInt32(in, static_cast<int32_t>(len));
    if (st != STATUS_OK) {
        printf("RESULT cpp %zu ERROR prepare=%d\n", len, st);
        AIBinder_decStrong(binder);
        return 1;
    }
    AParcel* out = nullptr;
    st = AIBinder_transact(binder, kGet, &in, &out, 0);
    if (st != STATUS_OK) {
        printf("RESULT cpp %zu ERROR transact=%d\n", len, st);
        AIBinder_decStrong(binder);
        return 1;
    }

    int rc = 1;
    int32_t got_len = 0;
    int32_t tag = 0;
    if (AParcel_readInt32(out, &got_len) != STATUS_OK ||
        AParcel_readInt32(out, &tag) != STATUS_OK) {
        printf("RESULT cpp %zu ERROR read-header\n", len);
    } else if (static_cast<size_t>(got_len) != len) {
        printf("RESULT cpp %zu ERROR length=%d\n", len, got_len);
    } else if (tag == kBlobAshmemImmutable || tag == kBlobAshmemMutable) {
        // The shared form: the tag is all this harness can check (see
        // the scope note). That rsbinder picked it for this size is
        // itself the claim being tested here.
        printf("RESULT cpp %zu SHARED %d\n", len, tag);
        rc = 0;
    } else if (tag != kBlobInplace) {
        printf("RESULT cpp %zu ERROR tag=%d\n", len, tag);
    } else {
        // Raw bytes out of the parcel: the payload is packed, which the
        // typed readers cannot express (see `write_inline_blob`).
        // Marshalling is safe here precisely because an inline blob
        // carries no fd and no binder.
        int32_t size = AParcel_getDataSize(out);
        std::vector<uint8_t> buf(size > 0 ? static_cast<size_t>(size) : 0);
        if (size < 8 || AParcel_marshal(out, buf.data(), 0, buf.size()) != STATUS_OK) {
            printf("RESULT cpp %zu ERROR marshal\n", len);
        } else if (get_i32(&buf[0]) != got_len || get_i32(&buf[4]) != kBlobInplace) {
            printf("RESULT cpp %zu ERROR header-mismatch\n", len);
        } else if (buf.size() < 8 + len) {
            printf("RESULT cpp %zu ERROR short=%zu\n", len, buf.size());
        } else {
            std::vector<uint8_t> payload(buf.begin() + 8, buf.begin() + 8 + len);
            if (payload != pattern(len)) {
                printf("RESULT cpp %zu ERROR payload-mismatch\n", len);
            } else {
                printf("RESULT cpp %zu INLINE %u\n", len, checksum(payload.data(), payload.size()));
                rc = 0;
            }
        }
    }

    AParcel_delete(out);
    AIBinder_decStrong(binder);
    return rc;
}

int main(int argc, char** argv) {
    if (argc == 3 && strcmp(argv[1], "server") == 0) return run_server(argv[2]);
    if (argc == 4 && strcmp(argv[1], "client") == 0) {
        return run_client(argv[2], strtoul(argv[3], nullptr, 10));
    }
    fprintf(stderr, "usage: %s client <service-name> <bytes> | server <service-name>\n", argv[0]);
    return 2;
}

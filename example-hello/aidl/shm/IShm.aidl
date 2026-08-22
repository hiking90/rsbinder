// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package shm;

/** Shared-memory example: a whole region plus dealer-backed frames. */
interface IShm {
    /**
     * The service's shared region as one fd — the same wire form as
     * `android.os.SharedMemory` / NDK `ASharedMemory`. Map it with
     * `SharedMemory::from_fd`.
     */
    ParcelFileDescriptor getRegion();

    /** Client → service signal: `[offset, offset+len)` of the region was written. */
    void regionWritten(int offset, int len);

    /**
     * A frame carved out of the service's `MemoryDealer` heap: an
     * `android.utils.IMemory` binder. Resolve it with `BpMemory` (ideally
     * through a `HeapCache`, so the heap is mapped only once).
     */
    IBinder nextFrame(int seq);
}

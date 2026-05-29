// SPDX-License-Identifier: Apache-2.0
//
// Plan 4-4 Phase D STAGE3 fixture — `TF_UPDATE_TXN` async dedup
// observed against the real Android binder driver.
//
// The recorder service stores every payload it actually delivered to
// its `onRecord` body; the client compares observed payloads against
// the values it sent to verify the kernel collapsed the stack of
// `FLAG_UPDATE_TXN` oneway calls into a single trailing entry.

package update_txn;

interface IUpdateTxnDedup {
    /**
     * Single-slot recorder. Each call appends to an internal Vec; with
     * `FLAG_UPDATE_TXN`, only the freshest call for a given
     * `(target, code)` should ever land while the server is busy with
     * an earlier transaction. Sleeps for `delay_ms` to widen the dedup
     * window deterministically.
     */
    oneway void onRecord(in int v, in int delay_ms);

    /** Drain — non-oneway so the client knows everything queued has run. */
    int[] drain();

    /** Reset the recorder before a fresh round. */
    void reset();
}

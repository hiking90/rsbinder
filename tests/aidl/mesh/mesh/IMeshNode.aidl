// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package mesh;

import mesh.MeshMessage;
import mesh.MeshValue;
import mesh.IMeshObserver;

// The mesh service interface, served by every node over whichever
// transport(s) it fronts. The same generated Bn*/Bp* stubs drive both
// kernel binder and RPC, so one impl + one client work over either.
interface IMeshNode {
    // Round-trip: returns `req` with seq+1 and origin/originKind rewritten
    // to this node's identity, so the caller can verify wire integrity
    // (blob/nonce echoed unchanged, seq incremented).
    MeshMessage exchange(in MeshMessage req);

    // Fold a union value into this node's running accumulator and return
    // the new total (i and l add; s adds its length).
    long accumulate(in MeshValue v);

    // Fire-and-forget delivery; the node forwards it to every registered
    // observer via IMeshObserver.onEvent.
    oneway void notify(in MeshMessage msg);

    // Register a callback binder (object passing across the wire).
    void registerObserver(IMeshObserver obs);

    // Total messages this node has received via exchange + notify.
    int receivedCount();
}

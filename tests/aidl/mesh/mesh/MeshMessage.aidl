// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package mesh;

import mesh.NodeKind;

// The payload exchanged across the mesh. `exchange` echoes it with a
// deterministic transform so the caller can verify round-trip integrity;
// `blob` exercises a variable-length byte array on the wire.
@RustDerive(Clone=true, PartialEq=true)
parcelable MeshMessage {
    int seq;
    long nonce;
    String origin;
    NodeKind originKind;
    byte[] blob;
}

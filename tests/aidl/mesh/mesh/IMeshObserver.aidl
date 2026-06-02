// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package mesh;

import mesh.MeshMessage;

// Callback registered by a peer; the node fires onEvent (oneway) for
// every message it receives, exercising binder-object passing + oneway
// dispatch over both transports.
interface IMeshObserver {
    oneway void onEvent(in MeshMessage msg);
}

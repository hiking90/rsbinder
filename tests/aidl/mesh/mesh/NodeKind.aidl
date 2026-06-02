// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package mesh;

// Which transport a mesh node primarily fronts. Carried in every
// MeshMessage so the receiver can assert it round-tripped intact.
@Backing(type="int")
enum NodeKind {
    KERNEL = 0,
    RPC = 1,
}

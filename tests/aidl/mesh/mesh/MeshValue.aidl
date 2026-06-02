// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

package mesh;

// A small tagged union, accumulated by IMeshNode.accumulate to exercise
// union wire encoding over both transports.
@RustDerive(Clone=true, PartialEq=true)
union MeshValue {
    int i;
    long l;
    String s;
}

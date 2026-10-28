// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Plan 16 fixture (`tests/lazy_descriptor_kernel.rs`): a node that counts the
// INTERFACE_TRANSACTIONs it serves, and receives other nodes' binders.
package lazydesc;

interface ILazyNode {
    // The INTERFACE_TRANSACTIONs this process has served so far.
    int interfaceQueries();

    // Reads `peer` as a bare IBinder.
    void takeBinder(IBinder peer);

    // Reads `peer` typed, then converts it every way plan 16 says makes no call.
    void takeTyped(ILazyNode peer);

    // `takeTyped` for a nullable argument.
    void takeNullable(@nullable ILazyNode peer);

    // Asks `peer` for its descriptor, on a binder thread that may make no twoway call.
    String askDescriptor(IBinder peer);
}

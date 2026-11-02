/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/** Where `ITopicUdsRegistry` tells a watcher of the sockets a name is given. */
oneway interface ITopicAddressWatcher {
    /**
     * `socket` is an `O_PATH` descriptor of the socket's file. `generation`
     * grows with every change of any name: a lower one than seen for `name` is
     * older and is dropped by the receiver.
     */
    void onAdvertised(String name, in ParcelFileDescriptor socket, long generation);
}

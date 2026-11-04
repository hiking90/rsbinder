/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

/**
 * Where `ITopicUdsRegistry` tells a watcher that a name was given a socket. A
 * client has one of these for all its names: the registry ends a session that
 * has not paid for the notices of one watcher and would owe for another.
 */
oneway interface ITopicAddressWatcher {
    /**
     * `name` has a new socket, which `lookup` gives. `generation` grows with
     * every change of any name. A notice carries no descriptor, so that one a
     * client has no room for costs it a call, not the session.
     */
    void onAdvertised(String name, long generation);
}

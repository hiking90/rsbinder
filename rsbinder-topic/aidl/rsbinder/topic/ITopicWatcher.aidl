/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.ITopic;

/** Where a registry tells a watcher of the topics a name is given. */
oneway interface ITopicWatcher {
    /**
     * `generation` grows with every change of any name in this registry: a
     * notice of a lower generation than one already seen for `name` is older
     * and is dropped by the receiver. A notice can come again, with the
     * topic the name has when it is sent again.
     */
    void onAdvertised(String name, ITopic topic, long generation);
}

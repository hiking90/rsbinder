/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.ITopic;
import rsbinder.topic.ITopicWatcher;
import rsbinder.topic.TopicInfo;
import rsbinder.topic.TopicRef;

/**
 * Names for topics, apart from service names: who may give a name to a
 * topic, who may find it, and who may list them, each by the host's policy.
 * The registry hands out binders and relays no subscription.
 *
 * A refusal by the policy is `EX_SECURITY`, a name outside the service name
 * rule (1 to 127 bytes of `[A-Za-z0-9._/-]`) is `EX_ILLEGAL_ARGUMENT`, and
 * the other refusals are the constants below. `lookup` answers a refused
 * caller as it answers a name with no topic.
 */
interface ITopicRegistry {
    /** The service name a host registers the registry under. */
    const String SERVICE_NAME = "rsbinder.topic.registry";

    /** ServiceSpecific: the name is held by a topic another uid advertised. */
    const int ERROR_TAKEN = 1;
    /** ServiceSpecific: the registry holds as many names or watchers as it takes. */
    const int ERROR_LIMIT = 2;

    /** Give `name` to `topic`, replacing a topic the caller's uid advertised under it. */
    void advertise(String name, ITopic topic);
    /** Take `name` back from `topic`; nothing if the name is not that topic's or not the caller's. */
    oneway void withdraw(String name, ITopic topic);
    /**
     * The topic `name` is given to, with the generation of that change; a
     * null topic also when the caller may not subscribe to it.
     */
    TopicRef lookup(String name);
    /**
     * Tell `watcher` of every topic `name` is given from now on; the topic it
     * has now, with the generation of that change, is the reply.
     */
    TopicRef watch(String name, ITopicWatcher watcher);
    void unwatch(String name, ITopicWatcher watcher);
    /** The names under `prefix` the caller may subscribe to, sorted. */
    TopicInfo[] list(String prefix);
}

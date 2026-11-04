/*
 * Copyright 2026 Jeff Kim <hiking90@gmail.com>
 * SPDX-License-Identifier: Apache-2.0
 */

package rsbinder.topic;

import rsbinder.topic.ITopic;
import rsbinder.topic.ITopicAddressWatcher;
import rsbinder.topic.TopicAddress;
import rsbinder.topic.TopicAllocation;
import rsbinder.topic.TopicInfo;

/**
 * Names for topics over RPC on a Unix domain socket. Where `ITopicRegistry`
 * hands out a topic's binder, which cannot leave the RPC session it arrived
 * on, this one hands out descriptors of a socket the registry made: a publisher
 * gets the bound socket and serves on it, and a subscriber gets an `O_PATH`
 * descriptor of its file, which has no name in the file system, and connects
 * through it. So who is given a descriptor is who can reach the topic.
 *
 * A refusal by the policy is `EX_SECURITY` and a name outside the service name
 * rule is `EX_ILLEGAL_ARGUMENT`. `lookup` answers a refused caller with no
 * socket and the generation `Long.MIN_VALUE`, as `ITopicRegistry` does. A call
 * that names an allocation that is gone, or a binder whose death the registry
 * cannot be told of, is `EX_ILLEGAL_STATE`.
 *
 * `topic` names the caller: a binder arrives as the same proxy on the session
 * that sent it, and no other session can make that proxy, so it tells who holds
 * an allocation. It is not a binder anyone else is given.
 */
interface ITopicUdsRegistry {
    /** ServiceSpecific: the name is held by a topic another uid advertised. */
    const int ERROR_TAKEN = 1;
    /** ServiceSpecific: the registry holds as many names or watchers as it takes. */
    const int ERROR_LIMIT = 2;

    /**
     * A bound socket for `name`, for the caller to listen on before it calls
     * `commit`. Nobody can find it until then. A second allocation of the same
     * name and `topic` takes the place of the first.
     */
    TopicAllocation allocate(String name, ITopic topic);
    /**
     * Give `name` to the allocation, in place of the topic it has. Who holds
     * the name is judged again here. A refusal leaves nothing behind: `allocate`
     * again to try again. The generation of the change is the reply.
     */
    long commit(String name, ITopic topic, long allocation);
    /** End the allocation, committed or not; nothing if it is not there. */
    oneway void withdraw(String name, ITopic topic, long allocation);
    /** The socket `name` is given to; none also when the caller may not subscribe. */
    TopicAddress lookup(String name);
    /**
     * Tell `watcher` whenever `name` is given a socket from now on, which `lookup`
     * gives. The name's generation is the reply. It brings no descriptor, so that
     * a process with none free can watch. A client passes one `watcher` for all
     * its names.
     */
    long watch(String name, ITopicAddressWatcher watcher);
    /**
     * End the watch of `name` by `watcher`; nothing if there is none. Twoway: a
     * `watch` of the name after it returns cannot arrive before it.
     */
    void unwatch(String name, ITopicAddressWatcher watcher);
    /** The names under `prefix` the caller may subscribe to, sorted. */
    TopicInfo[] list(String prefix);
}

/*
 * Copyright (C) 2006 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package android.os;

import android.os.IClientCallback;
import android.os.IServiceCallback;
import android.os.ServiceDebugInfo;

/**
 * Basic interface for finding and publishing system services.
 *
 * You likely want to use android.os.ServiceManager in Java or
 * android::IServiceManager in C++ in order to use this interface.
 *
 * @hide
 */
interface IServiceManager {
    /*
     * Must update values in IServiceManager.h
     */
    /* Allows services to dump sections according to priorities. */
    const int DUMP_FLAG_PRIORITY_CRITICAL = 1 << 0;
    const int DUMP_FLAG_PRIORITY_HIGH = 1 << 1;
    const int DUMP_FLAG_PRIORITY_NORMAL = 1 << 2;
    /**
     * Services are by default registered with a DEFAULT dump priority. DEFAULT priority has the
     * same priority as NORMAL priority but the services are not called with dump priority
     * arguments.
     */
    const int DUMP_FLAG_PRIORITY_DEFAULT = 1 << 3;

    const int DUMP_FLAG_PRIORITY_ALL =
             DUMP_FLAG_PRIORITY_CRITICAL | DUMP_FLAG_PRIORITY_HIGH
             | DUMP_FLAG_PRIORITY_NORMAL | DUMP_FLAG_PRIORITY_DEFAULT;

    /* Allows services to dump sections in protobuf format. */
    const int DUMP_FLAG_PROTO = 1 << 4;

    /**
     * Retrieve an existing service called @a name from the
     * service manager.
     *
     * This is the same as checkService (returns immediately) but
     * exists for legacy purposes.
     *
     * Returns null if the service does not exist.
     */
    @UnsupportedAppUsage
    @nullable IBinder getService(@utf8InCpp String name);

    /**
     * Retrieve an existing service called @a name from the service
     * manager. Non-blocking. Returns null if the service does not
     * exist.
     */
    @UnsupportedAppUsage
    @nullable IBinder checkService(@utf8InCpp String name);

    /**
     * Place a new @a service called @a name into the service
     * manager.
     */
    void addService(@utf8InCpp String name, IBinder service,
        boolean allowIsolated, int dumpPriority);

    /**
     * Return a list of all currently running services.
     */
    @utf8InCpp String[] listServices(int dumpPriority);

    /**
     * Request a callback when a service is registered.
     */
    void registerForNotifications(@utf8InCpp String name, IServiceCallback callback);

    /**
     * Unregisters all requests for notifications for a specific callback.
     */
    void unregisterForNotifications(@utf8InCpp String name, IServiceCallback callback);

    /**
     * Returns whether a given interface is declared on the device, even if it
     * is not started yet. For instance, this could be a service declared in the VINTF
     * manifest.
     */
    boolean isDeclared(@utf8InCpp String name);

    /**
     * Returns all declared instances for a particular interface.
     *
     * For instance, if 'android.foo.IFoo/foo' is declared, and 'android.foo.IFoo' is
     * passed here, then ["foo"] would be returned.
     */
    @utf8InCpp String[] getDeclaredInstances(@utf8InCpp String iface);

    /**
     * If updatable-via-apex, returns the APEX via which this is updated.
     */
    @nullable @utf8InCpp String updatableViaApex(@utf8InCpp String name);

    /**
     * Request a callback when the number of clients of the service changes.
     * Used by LazyServiceRegistrar to dynamically stop services that have no clients.
     */
    void registerClientCallback(@utf8InCpp String name, IBinder service, IClientCallback callback);

    /**
     * Attempt to unregister and remove a service. Will fail if the service is still in use.
     */
    void tryUnregisterService(@utf8InCpp String name, IBinder service);

    /**
     * Get debug information for all currently registered services.
     */
    ServiceDebugInfo[] getServiceDebugInfo();
}

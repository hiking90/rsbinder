/* //device/java/android/android/os/IPermissionController.aidl
**
** Copyright 2007, The Android Open Source Project
**
** Licensed under the Apache License, Version 2.0 (the "License");
** you may not use this file except in compliance with the License.
** You may obtain a copy of the License at
**
**     http://www.apache.org/licenses/LICENSE-2.0
**
** Unless required by applicable law or agreed to in writing, software
** distributed under the License is distributed on an "AS IS" BASIS,
** WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
** See the License for the specific language governing permissions and
** limitations under the License.
*/

package android.os;

/**
 * Client stub for system_server's PermissionManagerService.
 *
 * Vendored from
 * `frameworks/base/core/java/android/os/IPermissionController.aidl`
 * (AOSP android-16.0.0_r4). The wire descriptor
 * `"android.os.IPermissionController"` matches what
 * `BpPermissionController` writes via `Parcel::writeInterfaceToken`.
 *
 * rsbinder provides the client side only — the server lives in
 * Android's system_server (`PermissionManagerService`). Use via
 * `hub::get_service("permission")` from a process with permission to
 * reach system_server.
 *
 * @hide
 */
interface IPermissionController {
    boolean checkPermission(String permission, int pid, int uid);
    int noteOp(String op, int uid, String packageName);
    String[] getPackagesForUid(int uid);
    boolean isRuntimePermission(String permission);
    int getPackageUid(String packageName, int flags);
}

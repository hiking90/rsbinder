/*
 * Copyright (C) 2012 The Android Open Source Project
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

/**
 * Vendored verbatim from
 * `frameworks/base/core/java/android/os/ICancellationSignal.aidl`
 * (AOSP android-16.0.0_r4). The wire descriptor
 * `"android.os.ICancellationSignal"` is what an AOSP peer writes, so a
 * transport handed out by `rsbinder::cancel::CancellationSignal` is the
 * one a framework client already knows how to cancel.
 *
 * @hide
 */
interface ICancellationSignal {
    oneway void cancel();
}

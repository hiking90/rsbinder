/*
 * Copyright (C) 2020 The Android Open Source Project
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

package android.aidl.tests;

import android.aidl.tests.BadParcelable;
import android.aidl.tests.GenericStructuredParcelable;
import android.aidl.tests.IntEnum;
import android.aidl.tests.SimpleParcelable;
import android.aidl.tests.StructuredParcelable;
import android.aidl.tests.Union;
import android.os.PersistableBundle;

// Tests that are only supported by the C++/Java backends, not NDK/Rust
interface ICppJavaTests {
    BadParcelable RepeatBadParcelable(in BadParcelable input);

    SimpleParcelable RepeatSimpleParcelable(in SimpleParcelable input, out SimpleParcelable repeat);
    GenericStructuredParcelable<int, StructuredParcelable, IntEnum> RepeatGenericParcelable(
            in GenericStructuredParcelable<int, StructuredParcelable, IntEnum> input,
            out GenericStructuredParcelable<int, StructuredParcelable, IntEnum> repeat);
    PersistableBundle RepeatPersistableBundle(in PersistableBundle input);

    SimpleParcelable[] ReverseSimpleParcelables(
            in SimpleParcelable[] input, out SimpleParcelable[] repeated);
    PersistableBundle[] ReversePersistableBundles(
            in PersistableBundle[] input, out PersistableBundle[] repeated);
    Union ReverseUnion(in Union input, out Union repeated);
    // Test that List<T> types work correctly.
    List<IBinder> ReverseNamedCallbackList(in List<IBinder> input, out List<IBinder> repeated);

    FileDescriptor RepeatFileDescriptor(in FileDescriptor read);
    FileDescriptor[] ReverseFileDescriptorArray(
            in FileDescriptor[] input, out FileDescriptor[] repeated);
}

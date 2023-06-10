/*
 * Copyright (C) 2019 The Android Open Source Project
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

package android.aidl.tests.map;

import android.aidl.tests.map.Bar;
import android.aidl.tests.map.IEmpty;
import android.aidl.tests.map.IntEnum;

parcelable Foo {
    Map<String, IntEnum[]> intEnumArrayMap;
    Map<String, int[]> intArrayMap;
    Map<String, Bar> barMap;
    Map<String, Bar[]> barArrayMap;
    Map<String, String> stringMap;
    Map<String, String[]> stringArrayMap;
    Map<String, IEmpty> interfaceMap;
    Map<String, IBinder> ibinderMap;
}

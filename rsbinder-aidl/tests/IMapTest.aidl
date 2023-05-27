/*
 * Copyright (C) 2021 The Android Open Source Project
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

interface IMapTest {
    Map<String, IntEnum[]> repeatIntEnumArrayMap(
            in Map<String, IntEnum[]> input, out Map<String, IntEnum[]> output);
    Map<String, int[]> repeatIntArrayMap(
            in Map<String, int[]> input, out Map<String, int[]> output);
    Map<String, Bar> repeatBarMap(in Map<String, Bar> input, out Map<String, Bar> output);
    Map<String, Bar[]> repeatBarArrayMap(
            in Map<String, Bar[]> input, out Map<String, Bar[]> output);
    Map<String, String> repeatStringMap(
            in Map<String, String> input, out Map<String, String> output);
    Map<String, String[]> repeatStringArrayMap(
            in Map<String, String[]> input, out Map<String, String[]> output);
    Map<String, IEmpty> repeatInterfaceMap(
            in Map<String, IEmpty> input, out Map<String, IEmpty> output);
    Map<String, IBinder> repeatIbinderMap(
            in Map<String, IBinder> input, out Map<String, IBinder> output);
}

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

import android.aidl.tests.GenericStructuredParcelable;
import android.aidl.tests.IntEnum;
import android.aidl.tests.OtherParcelableForToString;
import android.aidl.tests.StructuredParcelable;
import android.aidl.tests.Union;

@JavaDerive(toString=true)
parcelable ParcelableForToString {
    int intValue;
    int[] intArray;
    long longValue;
    long[] longArray;
    double doubleValue;
    double[] doubleArray;
    float floatValue;
    float[] floatArray;
    byte byteValue;
    byte[] byteArray;
    boolean booleanValue;
    boolean[] booleanArray;
    String stringValue;
    String[] stringArray;
    List<String> stringList;
    OtherParcelableForToString parcelableValue;
    OtherParcelableForToString[] parcelableArray;
    IntEnum enumValue = IntEnum.FOO;
    IntEnum[] enumArray;
    String[] nullArray;
    List<String> nullList;
    GenericStructuredParcelable<int, StructuredParcelable, IntEnum> parcelableGeneric;
    Union unionValue;
}

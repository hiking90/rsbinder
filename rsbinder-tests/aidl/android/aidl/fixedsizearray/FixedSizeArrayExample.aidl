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

package android.aidl.fixedsizearray;

@JavaDerive(toString=true)
@RustDerive(PartialEq=true)
parcelable FixedSizeArrayExample {
    // to see if NxM array works
    int[2][3] int2x3 = {{1, 2, 3}, {4, 5, 6}};

    boolean[2] boolArray;
    byte[2] byteArray;
    char[2] charArray;
    int[2] intArray;
    long[2] longArray;
    float[2] floatArray;
    double[2] doubleArray;
    @utf8InCpp String[2] stringArray = {"hello", "world"};
    ByteEnum[2] byteEnumArray;
    IntEnum[2] intEnumArray;
    LongEnum[2] longEnumArray;
    IntParcelable[2] parcelableArray;
    // Following fields are not test-friendly because they should be initialized as non-null.
    // IBinder[2] binderArray;
    // ParcelFileDescriptor[2] pfdArray;
    // IEmptyInterface[2] interfaceArray;

    boolean[2][2] boolMatrix;
    byte[2][2] byteMatrix;
    char[2][2] charMatrix;
    int[2][2] intMatrix;
    long[2][2] longMatrix;
    float[2][2] floatMatrix;
    double[2][2] doubleMatrix;
    @utf8InCpp String[2][2] stringMatrix = {{"hello", "world"}, {"Ciao", "mondo"}};
    ByteEnum[2][2] byteEnumMatrix;
    IntEnum[2][2] intEnumMatrix;
    LongEnum[2][2] longEnumMatrix;
    IntParcelable[2][2] parcelableMatrix;
    // Following fields are not test-friendly because they should be initialized as non-null.
    // ParcelFileDescriptor[2][2] pfdMatrix;
    // IBinder[2][2] binderMatrix;
    // IEmptyInterface[2][2] interfaceMatrix;

    @nullable boolean[2] boolNullableArray;
    @nullable byte[2] byteNullableArray;
    @nullable char[2] charNullableArray;
    @nullable int[2] intNullableArray;
    @nullable long[2] longNullableArray;
    @nullable float[2] floatNullableArray;
    @nullable double[2] doubleNullableArray;
    @nullable @utf8InCpp String[2] stringNullableArray = {"hello", "world"};
    @nullable ByteEnum[2] byteEnumNullableArray;
    @nullable IntEnum[2] intEnumNullableArray;
    @nullable LongEnum[2] longEnumNullableArray;
    @nullable IBinder[2] binderNullableArray;
    @nullable ParcelFileDescriptor[2] pfdNullableArray;
    @nullable IntParcelable[2] parcelableNullableArray;
    @nullable IEmptyInterface[2] interfaceNullableArray;

    @nullable boolean[2][2] boolNullableMatrix;
    @nullable byte[2][2] byteNullableMatrix;
    @nullable char[2][2] charNullableMatrix;
    @nullable int[2][2] intNullableMatrix;
    @nullable long[2][2] longNullableMatrix;
    @nullable float[2][2] floatNullableMatrix;
    @nullable double[2][2] doubleNullableMatrix;
    @nullable @utf8InCpp String[2][2] stringNullableMatrix = {
            {"hello", "world"}, {"Ciao", "mondo"}};
    @nullable ByteEnum[2][2] byteEnumNullableMatrix;
    @nullable IntEnum[2][2] intEnumNullableMatrix;
    @nullable LongEnum[2][2] longEnumNullableMatrix;
    @nullable IBinder[2][2] binderNullableMatrix;
    @nullable ParcelFileDescriptor[2][2] pfdNullableMatrix;
    @nullable IntParcelable[2][2] parcelableNullableMatrix;
    @nullable IEmptyInterface[2][2] interfaceNullableMatrix;

    @SuppressWarnings(value={"out-array"})
    interface IRepeatFixedSizeArray {
        byte[3] RepeatBytes(in byte[3] input, out byte[3] repeated);
        int[3] RepeatInts(in int[3] input, out int[3] repeated);
        IBinder[3] RepeatBinders(in IBinder[3] input, out IBinder[3] repeated);
        IntParcelable[3] RepeatParcelables(
                in IntParcelable[3] input, out IntParcelable[3] repeated);

        byte[2][3] Repeat2dBytes(in byte[2][3] input, out byte[2][3] repeated);
        int[2][3] Repeat2dInts(in int[2][3] input, out int[2][3] repeated);
        IBinder[2][3] Repeat2dBinders(in IBinder[2][3] input, out IBinder[2][3] repeated);
        IntParcelable[2][3] Repeat2dParcelables(
                in IntParcelable[2][3] input, out IntParcelable[2][3] repeated);
    }

    enum ByteEnum { A }

    @Backing(type="int") enum IntEnum { A }

    @Backing(type="long") enum LongEnum { A }

    @JavaDerive(equals=true)
    @RustDerive(Clone=true, Copy=true, PartialEq=true)
    parcelable IntParcelable {
        int value;
    }

    interface IEmptyInterface {}
}

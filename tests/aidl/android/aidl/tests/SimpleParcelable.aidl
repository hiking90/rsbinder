/*
 * Copyright (C) 2015 The Android Open Source Project
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

// AOSP declares SimpleParcelable as an unstructured (custom rust_type)
// parcelable. rsbinder's test client and service are both rsbinder, so we
// declare it structured (name/number) — no AOSP wire-compat constraint here.
@JavaDerive(toString=true, equals=true)
@RustDerive(Clone=true, PartialEq=true)
parcelable SimpleParcelable {
    @utf8InCpp String name;
    int number;
}

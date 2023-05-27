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

package android.aidl.tests;

@SuppressWarnings(value={"inout-parameter", "out-nullable"})
parcelable ListOfInterfaces {
    interface IEmptyInterface {}

    interface IMyInterface {
        @nullable List<IEmptyInterface> methodWithInterfaces(IEmptyInterface iface,
                @nullable IEmptyInterface nullable_iface,
                in List<IEmptyInterface> iface_list_in, out List<IEmptyInterface> iface_list_out,
                inout List<IEmptyInterface> iface_list_inout,
                in @nullable List<IEmptyInterface> nullable_iface_list_in,
                out @nullable List<IEmptyInterface> nullable_iface_list_out,
                inout @nullable List<IEmptyInterface> nullable_iface_list_inout);
    }

    @JavaDerive(toString=true, equals=true)
    parcelable MyParcelable {
        IEmptyInterface iface;
        @nullable IEmptyInterface nullable_iface;
        List<IEmptyInterface> iface_list;
        @nullable List<IEmptyInterface> nullable_iface_list;
    }

    @JavaDerive(toString=true, equals=true)
    union MyUnion {
        IEmptyInterface iface;
        @nullable IEmptyInterface nullable_iface;
        List<IEmptyInterface> iface_list;
        @nullable List<IEmptyInterface> nullable_iface_list;
    }
}

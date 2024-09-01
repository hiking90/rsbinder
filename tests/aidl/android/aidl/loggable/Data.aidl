package android.aidl.loggable;

import android.aidl.loggable.Enum;
import android.aidl.loggable.Union;

parcelable Data {
    int num;
    @utf8InCpp String str;
    Union nestedUnion;
    Enum nestedEnum = Enum.FOO;
}

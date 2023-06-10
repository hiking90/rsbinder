package android.aidl.tests.immutable;

import android.aidl.tests.immutable.Bar;

@JavaOnlyImmutable
union Union {
    int num;
    String[] str;
    Bar bar;
}

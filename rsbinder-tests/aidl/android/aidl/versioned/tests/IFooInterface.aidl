package android.aidl.versioned.tests;
import android.aidl.versioned.tests.BazUnion;
import android.aidl.versioned.tests.Foo;

@JavaDelegator
interface IFooInterface {
    // V1
    void originalApi();
    @utf8InCpp String acceptUnionAndReturnString(in BazUnion u);
    @SuppressWarnings(value={"inout-parameter"})
    int ignoreParcelablesAndRepeatInt(in Foo inFoo, inout Foo inoutFoo, out Foo outFoo, int value);
    int returnsLengthOfFooArray(in Foo[] foos);
    // V2
    void newApi();
}

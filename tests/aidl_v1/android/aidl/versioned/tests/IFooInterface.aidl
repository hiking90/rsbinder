package android.aidl.versioned.tests;
import android.aidl.versioned.tests.BazUnion;
import android.aidl.versioned.tests.Foo;

// V1 (frozen) view of IFooInterface — identical package/name/descriptor to
// the V2 interface but WITHOUT the V2 `newApi()` method. The test service
// registers this under a dedicated name so a V2 client calling `newApi()`
// hits an unknown transaction code (4) → UNKNOWN_TRANSACTION, exactly as a
// real frozen-V1 service would behave. See plans/5-aosp-test-porting.md §4.
@JavaDelegator
interface IFooInterface {
    // V1
    void originalApi();
    @utf8InCpp String acceptUnionAndReturnString(in BazUnion u);
    @SuppressWarnings(value={"inout-parameter"})
    int ignoreParcelablesAndRepeatInt(in Foo inFoo, inout Foo inoutFoo, out Foo outFoo, int value);
    int returnsLengthOfFooArray(in Foo[] foos);
}

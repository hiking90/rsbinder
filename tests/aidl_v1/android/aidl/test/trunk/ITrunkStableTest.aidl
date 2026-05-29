package android.aidl.test.trunk;

// V1 (frozen) view — the SERVICE side of the trunk-stable cross-version
// tests. Same package/name/descriptor as the V2 interface but WITHOUT the
// V2 additions: `MyParcelable` has no `c`, `MyEnum` has no `THREE`, `MyUnion`
// has no `c`, and there is no `MyOtherParcelable` / `repeatOtherParcelable`
// (neither the top-level method nor the callback method). A V2 client talking
// to this V1 service exercises: field truncation (parcelable),
// unknown-enumerator round trip (enum), unknown-union-field rejection
// (union), and unknown-transaction (repeatOtherParcelable). See
// plans/5-aosp-test-porting.md §5.
interface ITrunkStableTest {
    @RustDerive(Clone=true, PartialEq=true)
    parcelable MyParcelable {
        int a;
        int b;
    }
    enum MyEnum {
        ZERO,
        ONE,
        TWO,
    }
    @RustDerive(Clone=true, PartialEq=true)
    union MyUnion {
        int a;
        int b;
    }
    interface IMyCallback {
        MyParcelable repeatParcelable(in MyParcelable input);
        MyEnum repeatEnum(in MyEnum input);
        MyUnion repeatUnion(in MyUnion input);
    }

    MyParcelable repeatParcelable(in MyParcelable input);
    MyEnum repeatEnum(in MyEnum input);
    MyUnion repeatUnion(in MyUnion input);
    void callMyCallback(in IMyCallback cb);
}

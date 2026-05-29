package android.aidl.test.trunk;

// V2 (current / "notfrozen") view — the CLIENT side of the trunk-stable
// cross-version tests. `MyParcelable.c`, `MyEnum.THREE`, `MyUnion.c`, and the
// whole `MyOtherParcelable` + `repeatOtherParcelable` (interface and callback)
// are new vs the frozen V1 the test service is built from. See
// plans/5-aosp-test-porting.md §5.
interface ITrunkStableTest {
    @RustDerive(Clone=true, PartialEq=true)
    parcelable MyParcelable {
        int a;
        int b;
        // New in V2
        int c;
    }
    enum MyEnum {
        ZERO,
        ONE,
        TWO,
        // New in V2
        THREE,
    }
    @RustDerive(Clone=true, PartialEq=true)
    union MyUnion {
        int a;
        int b;
        // New in V2
        int c;
    }
    interface IMyCallback {
        MyParcelable repeatParcelable(in MyParcelable input);
        MyEnum repeatEnum(in MyEnum input);
        MyUnion repeatUnion(in MyUnion input);
        MyOtherParcelable repeatOtherParcelable(in MyOtherParcelable input);
    }

    MyParcelable repeatParcelable(in MyParcelable input);
    MyEnum repeatEnum(in MyEnum input);
    MyUnion repeatUnion(in MyUnion input);
    void callMyCallback(in IMyCallback cb);

    // New in V2
    @RustDerive(Clone=true, PartialEq=true)
    parcelable MyOtherParcelable {
        int a;
        int b;
    }
    MyOtherParcelable repeatOtherParcelable(in MyOtherParcelable input);
}

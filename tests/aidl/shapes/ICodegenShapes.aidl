package shapes;

// Argument shapes the AOSP fixture corpus never uses; this interface is the
// only place they are exercised end to end.
interface ICodegenShapes {
    // `Vec<Option<T>>` sized by the caller; unset elements go back null, as AOSP's Rust backend.
    void takeOutBinders(in IBinder src, in boolean fill, out IBinder[] dst);

    // `@nullable` arrays of a primitive element are `Option<Vec<T>>` /
    // `Option<[T; N]>` — no per-element null marker.
    void roundNullableVec(inout @nullable int[] v);
    void roundNullableFixed(in @nullable int[3] v, out @nullable int[3] r);

    // A non-nullable `inout` array of a type with no `Default` is `Vec<T>`,
    // read from the parcel fully populated.
    void roundInoutBinders(inout IBinder[] v);

    // A `@FixedSize` union's `Tag` is byte-backed (AOSP parser.cpp
    // `UnionTagGenerater`), so `Tag[]` travels as a `byte[]`.
    @FixedSize union FixedTagged { int a; long b; }
    FixedTagged.Tag[] reverseTags(in FixedTagged.Tag[] tags);

    // The service returns `Status::from(StatusCode::UnexpectedNull)`; AOSP
    // `Status::fromStatusT` makes that EX_TRANSACTION_FAILED, i.e. the
    // transact status, not an EX_NULL_POINTER header.
    void failUnexpectedNull();

    // The service returns `DeadObject`; it is replied as FAILED_TRANSACTION,
    // because a DEAD_OBJECT reply makes AOSP `BpBinder::transact` mark the
    // live binder dead.
    void failDeadObject();
}

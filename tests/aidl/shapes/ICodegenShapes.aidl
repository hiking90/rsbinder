package shapes;

// Argument shapes whose generated Rust changed with the 2026-08 AIDL review.
// The AOSP fixture corpus uses none of them, so this interface is the only
// place they are exercised end to end.
interface ICodegenShapes {
    // A non-nullable `out` scalar with no `Default` is stored as `Option<T>`
    // and must be unwrapped into UNEXPECTED_NULL when the service leaves it
    // unset.
    void takeOutBinder(in IBinder src, in boolean fill, out IBinder dst);

    // `@nullable` arrays of a primitive element are `Option<Vec<T>>` /
    // `Option<[T; N]>` — no per-element null marker.
    void roundNullableVec(inout @nullable int[] v);
    void roundNullableFixed(in @nullable int[3] v, out @nullable int[3] r);

    // A non-nullable `inout` array of a type with no `Default` is `Vec<T>`,
    // read from the parcel fully populated.
    void roundInoutBinders(inout IBinder[] v);
}

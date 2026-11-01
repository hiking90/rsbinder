// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/// A type that can be an element of a [`MessageQueue`](crate::MessageQueue),
/// and that [`Bytes`](super::Bytes) copies in and out of shared memory.
///
/// The ring is memory another process writes, so an element is read from it
/// as raw bytes and must be meaningful whatever those bytes are.
///
/// # Safety
///
/// Every bit pattern of `size_of::<Self>()` bytes must be a valid `Self`;
/// `Self` must have no padding bytes (they would be copied out of and back
/// into shared memory as data) and an alignment of at most 8. The
/// primitive integers and floats satisfy this; `bool`, `char` and enums do
/// not. A `#[repr(C)]` struct of such fields with no padding does, which is
/// how AIDL `@FixedSize` parcelables become elements.
pub unsafe trait Element: Copy + 'static {}

macro_rules! elements {
    ($($t:ty),* $(,)?) => { $(
        // SAFETY: primitive numbers: no padding, every bit pattern valid, align <= 8.
        unsafe impl Element for $t {}
    )* };
}
elements!(u8, i8, u16, i16, u32, i32, u64, i64, f32, f64);

/// The bytes of a caller's slice of elements (never shared memory).
pub(crate) fn bytes_of<T: Element>(user: &[T]) -> &[u8] {
    // SAFETY: `Element` has no padding, so every byte of `user` is initialized; `u8` aligns to 1.
    unsafe { std::slice::from_raw_parts(user.as_ptr().cast(), std::mem::size_of_val(user)) }
}

/// The bytes of a caller's slice of elements (never shared memory), to fill.
pub(crate) fn bytes_of_mut<T: Element>(user: &mut [T]) -> &mut [u8] {
    // SAFETY: as in `bytes_of`, and any bytes written leave a valid `T` (the `Element` contract).
    unsafe { std::slice::from_raw_parts_mut(user.as_mut_ptr().cast(), std::mem::size_of_val(user)) }
}

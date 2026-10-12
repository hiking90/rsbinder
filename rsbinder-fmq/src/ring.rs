// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A caller's slice of elements as bytes, for copies to and from a [`Run`](crate::shm::Run).

use crate::queue::Element;

/// The bytes of a caller's slice of elements (never ring memory).
pub(crate) fn bytes_of<T: Element>(user: &[T]) -> &[u8] {
    // SAFETY: `Element` has no padding, so every byte of `user` is initialized; `u8` aligns to 1.
    unsafe { std::slice::from_raw_parts(user.as_ptr().cast(), std::mem::size_of_val(user)) }
}

/// The bytes of a caller's slice of elements (never ring memory), to fill.
pub(crate) fn bytes_of_mut<T: Element>(user: &mut [T]) -> &mut [u8] {
    // SAFETY: as in `bytes_of`, and any bytes written leave a valid `T` (the `Element` contract).
    unsafe { std::slice::from_raw_parts_mut(user.as_mut_ptr().cast(), std::mem::size_of_val(user)) }
}

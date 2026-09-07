// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! L2: the `BC_*`/`BR_*` ioctl buffer, native-endian by construction —
//! the private field makes the L1 wire codec unreachable from here.

use crate::{
    error::Result,
    parcel::{NativeScalar, Parcel},
    sys::{binder_transaction_data, binder_transaction_data_secctx},
};

pub(crate) struct CommandStream(Parcel);

impl CommandStream {
    pub(crate) fn new() -> Self {
        Self(Parcel::new())
    }

    /// L2 scalar: a `BC_*`/`BR_*` code, handle or cookie.
    pub(crate) fn write_cmd<T: NativeScalar>(&mut self, val: &T) -> Result<()> {
        self.0.write_native(val)
    }

    /// L2 scalar, as written by the driver.
    pub(crate) fn read_cmd<T: NativeScalar>(&mut self) -> Result<T> {
        self.0.read_native()
    }

    /// L3 UAPI struct, copied verbatim. Not generic: a `ParcelPod` bound
    /// would also admit the scalars, whose `Deserialize` is L1.
    pub(crate) fn write_transaction(&mut self, val: &binder_transaction_data) -> Result<()> {
        self.0.write_aligned(val)
    }

    /// L3 UAPI struct, as laid out by the driver.
    pub(crate) fn read_transaction(&mut self) -> Result<binder_transaction_data> {
        self.0.read()
    }

    /// L3 UAPI struct, as laid out by the driver.
    pub(crate) fn read_transaction_secctx(&mut self) -> Result<binder_transaction_data_secctx> {
        self.0.read()
    }

    pub(crate) fn data_size(&self) -> usize {
        self.0.data_size()
    }

    pub(crate) fn set_data_size(&mut self, new_len: usize) -> Result<()> {
        self.0.set_data_size(new_len)
    }

    /// # Safety
    ///
    /// Same contract as [`Parcel::set_data_size_driver_filled`]: bytes
    /// `0..new_len` must have been initialized by the driver.
    pub(crate) unsafe fn set_data_size_driver_filled(&mut self, new_len: usize) -> Result<()> {
        // SAFETY: this forwards the callee's obligation to this function's
        // own caller unchanged — see the `# Safety` section above.
        unsafe { self.0.set_data_size_driver_filled(new_len) }
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn capacity(&self) -> usize {
        self.0.capacity()
    }

    pub(crate) fn data_position(&self) -> usize {
        self.0.data_position()
    }

    pub(crate) fn set_data_position(&mut self, pos: usize) {
        self.0.set_data_position(pos)
    }

    /// The buffer the `BINDER_WRITE_READ` ioctl reads from / writes into.
    pub(crate) fn as_mut_ptr(&mut self) -> *mut u8 {
        self.0.as_mut_ptr()
    }
}

impl std::fmt::Debug for CommandStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

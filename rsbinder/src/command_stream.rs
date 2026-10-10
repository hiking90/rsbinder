// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! L2: the `BC_*`/`BR_*` ioctl buffer, native-endian by construction —
//! the private field makes the L1 wire codec unreachable from here.

use crate::{
    binder_object::bytes_at,
    error::Result,
    parcel::{NativeScalar, Parcel},
    transaction_data::{TransactionData, TransactionDataSecctx},
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

    /// L3 UAPI struct, in its own host-native codec (`transaction_data.rs`).
    pub(crate) fn write_transaction(&mut self, val: &TransactionData) -> Result<()> {
        self.0.write_aligned_data(&val.to_bytes())
    }

    /// L3 UAPI struct, as laid out by the driver.
    pub(crate) fn read_transaction(&mut self) -> Result<TransactionData> {
        let bytes = self.0.read_aligned_data(TransactionData::SIZE)?;
        Ok(TransactionData::from_bytes(&bytes_at(bytes, 0)))
    }

    /// L3 UAPI struct, as laid out by the driver.
    pub(crate) fn read_transaction_secctx(&mut self) -> Result<TransactionDataSecctx> {
        let bytes = self.0.read_aligned_data(TransactionDataSecctx::SIZE)?;
        Ok(TransactionDataSecctx::from_bytes(&bytes_at(bytes, 0)))
    }

    pub(crate) fn data_size(&self) -> usize {
        self.0.data_size()
    }

    pub(crate) fn set_data_size(&mut self, new_len: usize) -> Result<()> {
        self.0.set_data_size(new_len)
    }

    /// The queued commands, the bytes `BINDER_WRITE_READ` sends.
    pub(crate) fn as_bytes(&self) -> Result<&[u8]> {
        self.0.as_bytes()
    }

    /// What `BINDER_WRITE_READ` fills, zero past `data_size` ([`Parcel::driver_read_buffer`]).
    pub(crate) fn driver_read_buffer(&mut self) -> &mut [u8] {
        self.0.driver_read_buffer()
    }

    /// Keeps the `0..filled` bytes the driver wrote; see [`Parcel::driver_read_done`].
    pub(crate) fn driver_read_done(&mut self, filled: usize) -> Result<()> {
        self.0.driver_read_done(filled)
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn data_position(&self) -> usize {
        self.0.data_position()
    }

    pub(crate) fn set_data_position(&mut self, pos: usize) {
        self.0.set_data_position(pos)
    }
}

impl std::fmt::Debug for CommandStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

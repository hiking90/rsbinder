// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! UAPI `struct binder_transaction_data` and `binder_transaction_data_secctx`,
//! the L3 payloads of `BC_TRANSACTION`/`BC_REPLY` and `BR_TRANSACTION`/`BR_REPLY`.
//!
//! The command stream carries them host-native, as the driver lays them out:
//! `target`, `cookie`, `code`, `flags`, `sender_pid`, `sender_euid`,
//! `data_size`, `offsets_size` and the `data` union, at offsets 0, 8, 16, 20,
//! 24, 28, 32, 40 and 48 of 64 bytes; the secctx form appends the `secctx`
//! address at 64. `to_bytes`/`from_bytes` copy those fields, and compile-time
//! asserts tie the offsets to the bindgen structs.
//!
//! Neither union is a Rust union here. `target` (`__u32 handle` or
//! `binder_uintptr_t ptr`) is kept as its 8 bytes, with `handle` the first
//! four, as `FlatBinderObject` keeps its union (`binder_object.rs`):
//! `set_target_handle` zeroes the eight and writes the handle over the first
//! four, as AOSP does (`tr.target.ptr = 0; tr.target.handle = handle;`).
//! `data` is held as its `ptr` arm, `buffer` and `offsets`, which cover all 16
//! bytes; the driver reads and writes only that arm (the 8-byte `buf` arm is
//! unused by AOSP and by the driver).

use crate::sys::binder::{
    binder_size_t, binder_transaction_data, binder_transaction_data_secctx, binder_uintptr_t,
    pid_t, uid_t,
};

const _: () = {
    use std::mem::{offset_of, size_of};
    type Tr = binder_transaction_data;
    assert!(size_of::<Tr>() == TransactionData::SIZE);
    assert!(offset_of!(Tr, target) == 0 && offset_of!(Tr, cookie) == 8);
    assert!(offset_of!(Tr, code) == 16 && offset_of!(Tr, flags) == 20);
    assert!(offset_of!(Tr, sender_pid) == 24 && offset_of!(Tr, sender_euid) == 28);
    assert!(offset_of!(Tr, data_size) == 32 && offset_of!(Tr, offsets_size) == 40);
    assert!(offset_of!(Tr, data) == 48);
    assert!(size_of::<pid_t>() == 4 && size_of::<uid_t>() == 4);
    type Sec = binder_transaction_data_secctx;
    assert!(size_of::<Sec>() == TransactionDataSecctx::SIZE);
    assert!(offset_of!(Sec, secctx) == TransactionData::SIZE);
};

/// UAPI `struct binder_transaction_data`, host-native; see the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TransactionData {
    /// The `handle`/`ptr` union as its bytes: `handle` is the first four.
    target: [u8; 8],
    pub(crate) cookie: binder_uintptr_t,
    pub(crate) code: u32,
    pub(crate) flags: u32,
    pub(crate) sender_pid: pid_t,
    pub(crate) sender_euid: uid_t,
    pub(crate) data_size: binder_size_t,
    pub(crate) offsets_size: binder_size_t,
    /// `data.ptr.buffer`.
    pub(crate) buffer: binder_uintptr_t,
    /// `data.ptr.offsets`.
    pub(crate) offsets: binder_uintptr_t,
}

impl TransactionData {
    /// `sizeof(struct binder_transaction_data)`.
    pub(crate) const SIZE: usize = 64;

    /// `target.handle`: the receiver of an outgoing transaction.
    pub(crate) fn set_target_handle(&mut self, handle: u32) {
        self.target = [0; 8];
        self.target[..4].copy_from_slice(&handle.to_ne_bytes());
    }

    /// `target.ptr`: the local node an incoming transaction is for (a `publish_native` id).
    pub(crate) fn target_ptr(&self) -> binder_uintptr_t {
        binder_uintptr_t::from_ne_bytes(self.target)
    }

    #[cfg(test)]
    pub(crate) fn set_target_ptr(&mut self, ptr: binder_uintptr_t) {
        self.target = ptr.to_ne_bytes();
    }

    /// The 64 bytes the driver parses, host-native.
    pub(crate) fn to_bytes(self) -> [u8; Self::SIZE] {
        let mut bytes = [0; Self::SIZE];
        bytes[0..8].copy_from_slice(&self.target);
        bytes[8..16].copy_from_slice(&self.cookie.to_ne_bytes());
        bytes[16..20].copy_from_slice(&self.code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&self.flags.to_ne_bytes());
        bytes[24..28].copy_from_slice(&self.sender_pid.to_ne_bytes());
        bytes[28..32].copy_from_slice(&self.sender_euid.to_ne_bytes());
        bytes[32..40].copy_from_slice(&self.data_size.to_ne_bytes());
        bytes[40..48].copy_from_slice(&self.offsets_size.to_ne_bytes());
        bytes[48..56].copy_from_slice(&self.buffer.to_ne_bytes());
        bytes[56..64].copy_from_slice(&self.offsets.to_ne_bytes());
        bytes
    }

    /// The transaction in 64 host-native bytes; any bytes are a value.
    pub(crate) fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        // Fixed ranges of a 64-byte array, so every `try_into` succeeds.
        let u32_at = |at: usize| u32::from_ne_bytes(bytes[at..at + 4].try_into().unwrap());
        let u64_at = |at: usize| u64::from_ne_bytes(bytes[at..at + 8].try_into().unwrap());
        TransactionData {
            target: bytes[0..8].try_into().unwrap(),
            cookie: u64_at(8),
            code: u32_at(16),
            flags: u32_at(20),
            sender_pid: u32_at(24) as pid_t,
            sender_euid: u32_at(28),
            data_size: u64_at(32),
            offsets_size: u64_at(40),
            buffer: u64_at(48),
            offsets: u64_at(56),
        }
    }
}

/// UAPI `struct binder_transaction_data_secctx`: the transaction and its security context.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct TransactionDataSecctx {
    pub(crate) transaction_data: TransactionData,
    /// Address of the sender's NUL-terminated context, inside the receive mapping.
    pub(crate) secctx: binder_uintptr_t,
}

impl TransactionDataSecctx {
    /// `sizeof(struct binder_transaction_data_secctx)`.
    pub(crate) const SIZE: usize = TransactionData::SIZE + 8;

    /// The transaction and context address in 72 host-native bytes.
    pub(crate) fn from_bytes(bytes: &[u8; Self::SIZE]) -> Self {
        let (tr, secctx) = bytes.split_at(TransactionData::SIZE);
        TransactionDataSecctx {
            transaction_data: TransactionData::from_bytes(tr.try_into().unwrap()),
            secctx: binder_uintptr_t::from_ne_bytes(secctx.try_into().unwrap()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each field lands at its UAPI offset, on either byte order, and reads back.
    #[test]
    fn to_bytes_is_the_uapi_layout_and_round_trips() {
        let mut tr = TransactionData {
            cookie: 0x1111_2222_3333_4444,
            code: 0x5555_6666,
            flags: 0x10,
            sender_pid: -7,
            sender_euid: 1000,
            data_size: 0x40,
            offsets_size: 0x18,
            buffer: 0x7000_1000,
            offsets: 0x7000_2000,
            ..Default::default()
        };
        tr.set_target_handle(0xDEAD_BEEF);
        let bytes = tr.to_bytes();
        let mut target = [0u8; 8];
        target[..4].copy_from_slice(&0xDEAD_BEEFu32.to_ne_bytes());
        assert_eq!(bytes[0..8], target, "handle first, upper four zero");
        assert_eq!(bytes[16..20], 0x5555_6666u32.to_ne_bytes());
        assert_eq!(bytes[24..28], (-7i32).to_ne_bytes());
        assert_eq!(bytes[48..56], 0x7000_1000u64.to_ne_bytes());
        assert_eq!(bytes[56..64], 0x7000_2000u64.to_ne_bytes());
        assert_eq!(TransactionData::from_bytes(&bytes), tr);

        let mut secctx = [0u8; TransactionDataSecctx::SIZE];
        secctx[..TransactionData::SIZE].copy_from_slice(&bytes);
        secctx[TransactionData::SIZE..].copy_from_slice(&0xABCDu64.to_ne_bytes());
        let read = TransactionDataSecctx::from_bytes(&secctx);
        assert_eq!((read.transaction_data, read.secctx), (tr, 0xABCD));
    }

    #[test]
    fn target_ptr_reads_all_eight_bytes() {
        let mut tr = TransactionData::default();
        tr.set_target_ptr(0x0102_0304_0506_0708);
        assert_eq!(tr.target_ptr(), 0x0102_0304_0506_0708);
        tr.set_target_handle(9);
        let mut expect = [0u8; 8];
        expect[..4].copy_from_slice(&9u32.to_ne_bytes());
        assert_eq!(tr.target_ptr(), u64::from_ne_bytes(expect));
    }
}

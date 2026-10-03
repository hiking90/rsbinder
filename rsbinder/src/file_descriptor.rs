// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

/*
 * Copyright (C) 2020 The Android Open Source Project
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *      http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

//! File descriptor wrapper for binder IPC.
//!
//! This module provides `ParcelFileDescriptor`, a wrapper around file descriptors
//! that can be safely transmitted through binder IPC while maintaining proper
//! ownership semantics and automatic cleanup.
//!
//! # Raw fd wire
//!
//! `write_raw_fd` / `read_raw_fd` are AOSP `Parcel::writeFileDescriptor` /
//! `Parcel::readFileDescriptor`: the **bare** fd object with no not-null / comm
//! markers — `BINDER_TYPE_FD` on the kernel path, the fd-table index on an RPC
//! `Unix` fd-mode session (preceded by `TYPE_NATIVE_FILE_DESCRIPTOR`, with its
//! position recorded, on the android-13+ v1+ profile; bare on R34/v0). Writing
//! dups the fd (`F_DUPFD_CLOEXEC`); the caller keeps its own. Reading returns a dup of the parcel's object on the kernel path; on RPC it
//! returns the ancillary-table entry itself, **consumed** (a second read of the same
//! position is `BadValue`). `ParcelFileDescriptor` layers the AIDL markers on top of
//! this; handwritten AOSP interfaces such as `android.utils.IMemoryHeap` use the raw
//! form directly.

use crate::error::{Result, StatusCode};
use crate::{
    Deserialize, DeserializeArray, DeserializeOption, Parcel, Serialize, SerializeArray,
    SerializeOption,
};

use std::os::unix::io::{AsFd, AsRawFd, BorrowedFd, IntoRawFd, OwnedFd, RawFd};

/// File descriptor wrapper for binder IPC.
///
/// `ParcelFileDescriptor` is a Rust equivalent of the Java `android.os.ParcelFileDescriptor`,
/// providing safe transmission of file descriptors through binder IPC while ensuring
/// proper ownership and automatic cleanup.
#[derive(Debug)]
pub struct ParcelFileDescriptor(OwnedFd);

impl ParcelFileDescriptor {
    /// Create a new `ParcelFileDescriptor` from any type that can be converted to `OwnedFd`.
    ///
    /// For the common concrete cases prefer the `From` impls
    /// (`ParcelFileDescriptor::from(owned_fd)` / `from(file)`); this generic
    /// constructor covers any other `Into<OwnedFd>` source.
    pub fn new<F: Into<OwnedFd>>(fd: F) -> Self {
        Self(fd.into())
    }

    /// Duplicate the underlying file descriptor into a new owned
    /// `ParcelFileDescriptor`. The new fd is `O_CLOEXEC` (`fcntl(F_DUPFD_CLOEXEC)`,
    /// which always sets it regardless of the source's flag), matching AOSP
    /// `ParcelFileDescriptor::dup`. Useful when you must both keep an fd and
    /// hand a copy to a parcel.
    pub fn try_clone(&self) -> Result<Self> {
        let dup = rustix::io::fcntl_dupfd_cloexec(&self.0, 0)?;
        Ok(Self(dup))
    }

    /// A new pipe as two `ParcelFileDescriptor`s, `(read, write)` —
    /// AOSP `ParcelFileDescriptor.createPipe()`.
    ///
    /// This is how a large or open-ended payload crosses binder: the
    /// parcel carries one end as a file descriptor and the bytes go
    /// through the kernel pipe, so neither side has to size a buffer or
    /// fit the whole thing in a transaction.
    ///
    /// ```no_run
    /// # use rsbinder::*;
    /// # use std::io::Write;
    /// # fn f() -> Result<()> {
    /// let (read_end, write_end) = ParcelFileDescriptor::pipe()?;
    /// // Hand `read_end` to the peer in a parcel, then fill the pipe:
    /// (&write_end).write_all(b"...")?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Both ends are `O_CLOEXEC`, as AOSP's are: an fd on its way into a
    /// parcel must not leak into a child this process happens to
    /// `exec` in between.
    ///
    /// **A pipe holds about 64 KB before it blocks.** Whoever writes
    /// more than that must have a reader already draining it — usually
    /// the peer, once the read end has been sent, or another thread.
    /// Writing the whole payload before sending the read end works only
    /// while it fits the buffer.
    pub fn pipe() -> Result<(Self, Self)> {
        // Apple has no pipe2: CLOEXEC goes on afterwards, racing only an `exec` on another thread.
        #[cfg(not(target_vendor = "apple"))]
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        #[cfg(target_vendor = "apple")]
        let (read, write) = {
            let (read, write) = rustix::pipe::pipe()?;
            rustix::io::fcntl_setfd(&read, rustix::io::FdFlags::CLOEXEC)?;
            rustix::io::fcntl_setfd(&write, rustix::io::FdFlags::CLOEXEC)?;
            (read, write)
        };
        Ok((Self(read), Self(write)))
    }
}

/// Read from the descriptor, as [`std::fs::File`] does — including
/// `Ok(0)` for end of file, which for a pipe means every writer is gone.
///
/// Both this and the `&ParcelFileDescriptor` impl exist for the same
/// reason `File` has both: a reader that owns the descriptor writes
/// `pfd.read(..)`, and one that only borrowed it writes
/// `(&pfd).read(..)`.
///
/// For `tokio`, convert once: `tokio::fs::File::from_std(
/// std::fs::File::from(OwnedFd::from(pfd)))`. rsbinder does not wrap
/// that — its `tokio` feature deliberately does not pull `tokio/fs`.
impl std::io::Read for ParcelFileDescriptor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        (&*self).read(buf)
    }
}

impl std::io::Read for &ParcelFileDescriptor {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        Ok(rustix::io::read(&self.0, buf)?)
    }
}

/// Write to the descriptor, as [`std::fs::File`] does — including
/// `EPIPE` once the reading end is closed. [`flush`](std::io::Write::flush)
/// is a no-op: there is no buffer here, and the pipe's own is the
/// kernel's.
impl std::io::Write for ParcelFileDescriptor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        (&*self).write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Write for &ParcelFileDescriptor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        Ok(rustix::io::write(&self.0, buf)?)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl AsRef<OwnedFd> for ParcelFileDescriptor {
    fn as_ref(&self) -> &OwnedFd {
        &self.0
    }
}

impl AsFd for ParcelFileDescriptor {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl From<ParcelFileDescriptor> for OwnedFd {
    fn from(fd: ParcelFileDescriptor) -> OwnedFd {
        fd.0
    }
}

impl From<OwnedFd> for ParcelFileDescriptor {
    fn from(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

impl From<std::fs::File> for ParcelFileDescriptor {
    fn from(file: std::fs::File) -> Self {
        Self(file.into())
    }
}

impl AsRawFd for ParcelFileDescriptor {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for ParcelFileDescriptor {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl PartialEq for ParcelFileDescriptor {
    // Each PFD owns its fd, so `true` for two distinct objects means an fd is double-owned.
    fn eq(&self, other: &Self) -> bool {
        self.as_raw_fd() == other.as_raw_fd()
    }
}

impl Eq for ParcelFileDescriptor {}

/// The RPC fd body a parcel carries; only `rpc` builds construct it (hence the `allow`).
#[cfg_attr(not(feature = "rpc"), allow(dead_code))]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RpcFdProfile {
    /// R34 / v0: rsbinder-only bare ancillary index (AOSP forbids fd-over-RPC there).
    V0,
    /// android-13+ v1+: AOSP `TYPE_NATIVE_FILE_DESCRIPTOR` + index, position recorded (plan/2-11).
    V1Plus,
}

/// `None` = kernel `BINDER_TYPE_FD`; `FdsNotAllowed` = no fd mode negotiated (RPC or data-only).
fn rpc_fd_profile(parcel: &Parcel) -> Result<Option<RpcFdProfile>> {
    if parcel.is_kernel_backed() {
        return Ok(None);
    }
    #[cfg(feature = "rpc")]
    {
        use crate::rpc::FileDescriptorTransportMode as M;
        match parcel.rpc_fd_mode() {
            M::None => Err(StatusCode::FdsNotAllowed),
            M::Unix if parcel.rpc_record_fd_positions() => Ok(Some(RpcFdProfile::V1Plus)),
            M::Unix => Ok(Some(RpcFdProfile::V0)),
        }
    }
    #[cfg(not(feature = "rpc"))]
    Err(StatusCode::FdsNotAllowed)
}

/// AOSP `Parcel::writeFileDescriptor`: bare fd object, fd dup'd; see module doc "Raw fd wire".
pub(crate) fn write_raw_fd(parcel: &mut Parcel, fd: BorrowedFd<'_>) -> Result<()> {
    write_raw_owned_fd(parcel, dup_for_parcel(parcel, fd)?)
}

/// Fd-mode gate + dup, run before writing anything so a failure leaves the parcel untouched.
fn dup_for_parcel(parcel: &Parcel, fd: BorrowedFd<'_>) -> Result<OwnedFd> {
    rpc_fd_profile(parcel)?;
    Ok(rustix::io::fcntl_dupfd_cloexec(fd, 0)?)
}

/// Body of [`write_raw_fd`] for a [`dup_for_parcel`] fd; ownership moves only after the body.
fn write_raw_owned_fd(parcel: &mut Parcel, dup: OwnedFd) -> Result<()> {
    #[cfg(feature = "rpc")]
    if let Some(profile) = rpc_fd_profile(parcel)? {
        // Push only after the body is written, so a failed write leaves no ghost table entry.
        let idx = parcel.rpc_out_fds().len() as i32;
        match profile {
            RpcFdProfile::V1Plus => {
                // AOSP `writeFileDescriptor`: the recorded position is the TYPE offset (plan/2-11).
                let obj_pos = parcel.data_position();
                parcel.write::<i32>(&crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR)?;
                parcel.write::<i32>(&idx)?;
                parcel.rpc_record_object_position(obj_pos);
            }
            RpcFdProfile::V0 => parcel.write::<i32>(&idx)?,
        }
        let pushed = parcel.rpc_push_out_fd(dup);
        debug_assert_eq!(pushed, idx);
        return Ok(());
    }

    parcel.write_kernel_fd(dup)
}

/// AOSP `Parcel::readFileDescriptor`: RPC consumes the table entry; see module doc "Raw fd wire".
pub(crate) fn read_raw_fd(parcel: &mut Parcel) -> Result<OwnedFd> {
    // Refuses a parcel whose fd mode forbids fds before any of it is read.
    let profile = rpc_fd_profile(parcel)?;
    #[cfg(not(feature = "rpc"))]
    let _ = profile;
    #[cfg(feature = "rpc")]
    if let Some(profile) = profile {
        if profile == RpcFdProfile::V1Plus {
            // AOSP readFileDescriptor: object-position miss ⇒ BAD_TYPE, v1 and v2 (plan/2-11).
            let pos = parcel.data_position();
            if !parcel.rpc_object_position_present(pos) {
                return Err(StatusCode::BadType);
            }
            let ty = parcel.read::<i32>()?;
            if ty != crate::rpc::wire_android13::TYPE_NATIVE_FILE_DESCRIPTOR {
                return Err(StatusCode::BadType);
            }
        }
        let idx = parcel.read::<i32>()?;
        if idx < 0 {
            return Err(StatusCode::BadValue);
        }
        return parcel
            .rpc_take_in_fd(idx as usize)
            .ok_or(StatusCode::BadValue);
    }

    parcel.read_kernel_fd_dup()
}

impl Serialize for ParcelFileDescriptor {
    fn serialize(&self, parcel: &mut Parcel) -> Result<()> {
        // `[1][hasComm=0][fd]` (`AParcel_writeParcelFileDescriptor`); R34/v0 RPC: `[1][idx]`.
        let dup = dup_for_parcel(parcel, self.0.as_fd())?;
        #[cfg(feature = "rpc")]
        if let Some(profile) = rpc_fd_profile(parcel)? {
            parcel.write::<i32>(&1)?; // not-null marker / present
            if profile == RpcFdProfile::V1Plus {
                parcel.write::<i32>(&0)?; // hasComm = 0 (no comm fd)
            }
            return write_raw_owned_fd(parcel, dup);
        }

        // Not null
        parcel.write::<i32>(&1)?;
        parcel.write::<i32>(&0)?;
        write_raw_owned_fd(parcel, dup)
    }
}

impl SerializeArray for ParcelFileDescriptor {}

impl SerializeOption for ParcelFileDescriptor {
    fn serialize_option(this: Option<&Self>, parcel: &mut Parcel) -> Result<()> {
        if let Some(f) = this {
            f.serialize(parcel)
        } else {
            parcel.write::<i32>(&0)
        }
    }
}

impl DeserializeOption for ParcelFileDescriptor {
    fn deserialize_option(parcel: &mut Parcel) -> Result<Option<Self>> {
        // Null fd = `writeInt32(0)` on every profile (AOSP); the body mirrors `serialize`.
        let present = parcel.read::<i32>()?;
        if present == crate::NULL_PARCELABLE_FLAG {
            return Ok(None);
        }
        // AOSP `Parcel::readData(Parcelable*)`: any marker but `1` is `UNEXPECTED_NULL`.
        if present != crate::NON_NULL_PARCELABLE_FLAG {
            return Err(StatusCode::UnexpectedNull);
        }

        let profile = rpc_fd_profile(parcel)?;
        #[cfg(not(feature = "rpc"))]
        let _ = profile;
        #[cfg(feature = "rpc")]
        if let Some(profile) = profile {
            if profile == RpcFdProfile::V1Plus {
                // No comm channel here: non-zero hasComm is BadValue (libbinder always writes 0).
                let has_comm = parcel.read::<i32>()?;
                if has_comm != 0 {
                    return Err(StatusCode::BadValue);
                }
            }
            return Ok(Some(ParcelFileDescriptor::new(read_raw_fd(parcel)?)));
        }

        // Java PFD `writeToParcel`: `[hasComm][fd]`, plus `[commFd]` when hasComm != 0 (reliable).
        let has_comm = parcel.read::<i32>()?;
        let fd = read_raw_fd(parcel)?;

        // Reliable-PFD comm socket: consume it, send `DETACHED` (AOSP `readParcelFileDescriptor`).
        if has_comm != 0 {
            parcel.read_kernel_fd_with(send_detached)?;
        }

        Ok(Some(ParcelFileDescriptor::new(fd)))
    }
}

/// Tell a reliable PFD's sender on `comm_fd` that this end detached; failures are only logged.
fn send_detached(comm_fd: BorrowedFd<'_>) {
    // Java PFD comm channel, not parcel wire: AOSP peeks this int BIG_ENDIAN.
    const DETACHED: i32 = 2;
    let notice = DETACHED.to_be_bytes();
    // A sender that already closed its end must not fail the fd: AOSP only logs.
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let flags = rustix::net::SendFlags::NOSIGNAL;
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let flags = rustix::net::SendFlags::empty();
    loop {
        match rustix::net::send(comm_fd, &notice, flags) {
            Ok(n) if n == notice.len() => break,
            Ok(n) => {
                log::error!("short write of the DETACHED status to the comm fd: {n} bytes");
                break;
            }
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => {
                log::error!("failed to write the DETACHED status to the comm fd: {e}");
                break;
            }
        }
    }
}

impl Deserialize for ParcelFileDescriptor {
    fn deserialize(parcel: &mut Parcel) -> Result<Self> {
        Deserialize::deserialize(parcel)
            .transpose()
            .unwrap_or(Err(StatusCode::UnexpectedNull))
    }
}

impl DeserializeArray for ParcelFileDescriptor {}

/// Fuzz entrypoint for the `rpc_fd_ancillary` target: arbitrary body
/// bytes through the RPC `Unix`-mode FD-table decode **with no received
/// fds**. Property: no panic / UB / fd leak — an out-of-bounds or
/// dangling fd-table index is a clean `Err`, never a crash. Not part of
/// the supported API surface.
#[cfg(all(feature = "rpc", feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_rpc_fd_index(input: &[u8]) {
    let mut p = Parcel::data_only_from_vec(input.to_vec());
    p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
    // No ancillary fds installed: every index must be rejected, not panic / leak.
    let _ = <ParcelFileDescriptor as DeserializeOption>::deserialize_option(&mut p);
}

/// Fuzz entrypoint for the **v1+ AOSP-shape** RPC FD decode: arbitrary
/// body bytes + a hostile object-position table through the strict
/// `[not-null|hasComm|TYPE|idx]` +
/// `binary_search` read, **no received fds**. Property: no panic / UB /
/// fd leak — a forged/unsorted/absent position, a wrong `TYPE`, a
/// non-zero `hasComm`, or a dangling index is a clean `Err`, never a
/// crash. Complements [`__fuzz_rpc_fd_index`] (which only covers the
/// R34 legacy `[present|idx]` path). Not part of the supported API.
#[cfg(all(feature = "rpc", feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_rpc_fd_index_v1(input: &[u8]) {
    let mut p = fuzz_v1_parcel(input);
    // No ancillary fds installed: every index must be rejected, not panic / leak.
    let _ = <ParcelFileDescriptor as DeserializeOption>::deserialize_option(&mut p);
}

/// v1+ fuzz input: first byte = count of leading u32 hostile object positions; rest = body.
#[cfg(all(feature = "rpc", feature = "fuzzing"))]
fn fuzz_v1_parcel(input: &[u8]) -> Parcel {
    let (n_pos, rest) = match input.split_first() {
        Some((&n, rest)) => ((n % 16) as usize, rest),
        None => (0, input),
    };
    let take = (n_pos * 4).min(rest.len());
    let positions: Vec<u32> = rest[..take]
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let mut p = Parcel::data_only_from_vec(rest[take..].to_vec());
    p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
    p.set_rpc_record_fd_positions(true); // v1+ AOSP body + strict read
    p.rpc_set_object_positions(positions);
    p
}

/// Fuzz entrypoint for the **bare** fd decode ([`read_raw_fd`], the
/// `android.utils.IMemoryHeap` wire) over RPC `Unix` fd-mode, **no
/// received fds**. First byte selects the profile: even = R34/v0 bare
/// index, odd = v1+ `[TYPE|idx]` with the next byte sizing a hostile
/// object-position table (as in [`__fuzz_rpc_fd_index_v1`]). Property:
/// no panic / UB / fd leak — every forged position, type, or index is
/// a clean `Err`. Not part of the supported API.
#[cfg(all(feature = "rpc", feature = "fuzzing"))]
#[doc(hidden)]
pub fn __fuzz_rpc_raw_fd(input: &[u8]) {
    let Some((&profile, rest)) = input.split_first() else {
        return;
    };
    let mut p = if profile % 2 == 0 {
        let mut p = Parcel::data_only_from_vec(rest.to_vec());
        p.set_rpc_fd_mode(crate::rpc::FileDescriptorTransportMode::Unix);
        p
    } else {
        fuzz_v1_parcel(rest)
    };
    let _ = read_raw_fd(&mut p);
}

#[cfg(test)]
mod tests {
    use std::os::fd::FromRawFd;

    use super::*;

    /// Plan 10-3 AC-3.4: the adapters behave as `std::fs::File`'s do, which callers assume.
    #[test]
    fn a_pipe_reads_and_writes_like_a_file() {
        use std::io::{Read, Write};

        let (read_end, write_end) = ParcelFileDescriptor::pipe().expect("pipe");

        // Close-on-exec: no end leaks into a child exec'd while the fd is on its way to a parcel.
        for end in [&read_end, &write_end] {
            let flags = rustix::io::fcntl_getfd(end).expect("F_GETFD");
            assert!(
                flags.contains(rustix::io::FdFlags::CLOEXEC),
                "a pipe end must be O_CLOEXEC"
            );
        }

        (&write_end).write_all(b"hello").expect("write_all");
        let mut buf = [0u8; 5];
        (&read_end).read_exact(&mut buf).expect("read_exact");
        assert_eq!(&buf, b"hello");

        // End of file is `Ok(0)`, as for a `File`: every writing end is gone.
        drop(write_end);
        let mut rest = Vec::new();
        (&read_end).read_to_end(&mut rest).expect("read_to_end");
        assert!(rest.is_empty());

        // Writing to a pipe nobody reads is `EPIPE`; the Rust runtime ignores `SIGPIPE`.
        let (read_end, write_end) = ParcelFileDescriptor::pipe().expect("pipe");
        drop(read_end);
        let err = (&write_end)
            .write_all(b"x")
            .expect_err("a broken pipe fails");
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn test_parcel_file_descriptor() {
        // Not stdout: std owns fd 1, and a failing assert would close it during unwind.
        let f = std::fs::File::open("/dev/null").expect("/dev/null");
        let raw = f.as_raw_fd();
        let pfd = ParcelFileDescriptor::from(f);
        assert_eq!(pfd.as_raw_fd(), raw);

        let owned_fd: OwnedFd = pfd.into();
        let pfd = ParcelFileDescriptor::new(owned_fd);
        assert_eq!(pfd.into_raw_fd(), raw);

        // SAFETY: `into_raw_fd` just gave up `raw`, so nothing else owns it.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
    }

    #[test]
    fn test_pfd_conversions_and_try_clone() {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("/dev/null");
        // From<std::fs::File>
        let pfd: ParcelFileDescriptor = f.into();
        // AsFd delegates to the inner OwnedFd.
        let _borrowed: BorrowedFd<'_> = pfd.as_fd();
        assert!(pfd.as_raw_fd() >= 0);

        // try_clone dups (O_CLOEXEC) into a distinct fd.
        let cloned = pfd.try_clone().expect("try_clone");
        assert_ne!(
            pfd.as_raw_fd(),
            cloned.as_raw_fd(),
            "dup must allocate a new fd"
        );

        // From<OwnedFd> round-trips through OwnedFd.
        let owned: OwnedFd = cloned.into();
        let _pfd2: ParcelFileDescriptor = owned.into();
    }

    // FD-over-RPC body goldens + strict-read mutants; socket round-trip: `rpc_fd::fd_v1plus_*`.

    #[cfg(feature = "rpc")]
    fn dev_null_pfd() -> ParcelFileDescriptor {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .open("/dev/null")
            .expect("/dev/null");
        ParcelFileDescriptor::new(f)
    }

    #[cfg(feature = "rpc")]
    fn rpc_parcel(record_fd_positions: bool) -> Parcel {
        use crate::rpc::FileDescriptorTransportMode as M;
        let mut p = Parcel::new();
        p.set_for_rpc(true).unwrap();
        p.set_rpc_fd_mode(M::Unix);
        p.set_rpc_record_fd_positions(record_fd_positions);
        p
    }

    /// v1+ writes AOSP `[1][hasComm=0][TYPE=2][idx=0]` (16 B), recording the TYPE offset (+8).
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_v1_body_golden() {
        let mut p = rpc_parcel(true);
        dev_null_pfd().serialize(&mut p).expect("serialize v1+ fd");
        let mut want = Vec::new();
        want.extend_from_slice(&1i32.to_le_bytes()); // not-null
        want.extend_from_slice(&0i32.to_le_bytes()); // hasComm = 0
        want.extend_from_slice(&2i32.to_le_bytes()); // TYPE_NATIVE_FILE_DESCRIPTOR
        want.extend_from_slice(&0i32.to_le_bytes()); // fdIndex (first out fd)
        assert_eq!(p.rpc_data_bytes(), &want[..], "v1+ AOSP fd body");
        assert_eq!(
            p.rpc_object_positions(),
            &[8u32],
            "recorded position = TYPE int32 offset (after not-null+hasComm), not +0"
        );
    }

    /// R34 / v0 writes the AOSP android-12 layout `[present=1][fdIndex=0]` (8 B), no position.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_r34_body_byte_unchanged() {
        let mut p = rpc_parcel(false);
        dev_null_pfd().serialize(&mut p).expect("serialize r34 fd");
        let mut want = Vec::new();
        want.extend_from_slice(&1i32.to_le_bytes()); // present
        want.extend_from_slice(&0i32.to_le_bytes()); // fdIndex
        assert_eq!(p.rpc_data_bytes(), &want[..], "R34 legacy fd body");
        assert!(
            p.rpc_object_positions().is_empty(),
            "R34/v0 has no object table"
        );
    }

    /// A null fd is `writeInt32(0)` at both profiles: no TYPE, and no position even at v1+.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_null_body_unchanged_both_profiles() {
        for record in [false, true] {
            let mut p = rpc_parcel(record);
            <ParcelFileDescriptor as SerializeOption>::serialize_option(None, &mut p)
                .expect("serialize None fd");
            assert_eq!(
                p.rpc_data_bytes(),
                &0i32.to_le_bytes()[..],
                "null fd = writeInt32(0) (record_fd_positions={record})"
            );
            assert!(
                p.rpc_object_positions().is_empty(),
                "null fd records no position (record_fd_positions={record})"
            );
        }
    }

    #[cfg(feature = "rpc")]
    fn v1_reader(body: &[u8], positions: Vec<u32>) -> Parcel {
        use crate::rpc::FileDescriptorTransportMode as M;
        let mut p = Parcel::data_only_from_vec(body.to_vec());
        p.set_rpc_fd_mode(M::Unix);
        p.set_rpc_record_fd_positions(true);
        p.rpc_set_object_positions(positions);
        p.set_data_position(0);
        p
    }

    /// Forged v1+ bodies are clean `Err`s; explicit, as a symmetric round trip cannot catch them.
    #[cfg(feature = "rpc")]
    #[test]
    fn rpc_fd_v1_strict_read_rejects_mutants() {
        let ok_body = {
            let mut b = Vec::new();
            b.extend_from_slice(&1i32.to_le_bytes()); // not-null
            b.extend_from_slice(&0i32.to_le_bytes()); // hasComm
            b.extend_from_slice(&2i32.to_le_bytes()); // TYPE
            b.extend_from_slice(&0i32.to_le_bytes()); // idx
            b
        };
        let de = <ParcelFileDescriptor as DeserializeOption>::deserialize_option;

        // (1) legacy R34 `[present=1][idx=7]` to a v1+ reader: idx is read as hasComm != 0.
        let mut legacy = Vec::new();
        legacy.extend_from_slice(&1i32.to_le_bytes());
        legacy.extend_from_slice(&7i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&legacy, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "legacy [present|idx] vs v1+ reader (hasComm!=0)"
        );

        // (2) position missing from the object table ⇒ BadType (AOSP: binary_search miss).
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![])).unwrap_err(),
            StatusCode::BadType,
            "unrecorded fd position rejected (strict v1+)"
        );

        // (3) position at +0 (not-null marker), not the +8 TYPE offset: obj_pos taken too early.
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![0])).unwrap_err(),
            StatusCode::BadType,
            "position must point at the TYPE int32 (start+8), not +0"
        );

        // (4) wrong object type (TYPE_BINDER=1 instead of 2).
        let mut wrong_ty = ok_body.clone();
        wrong_ty[8..12].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&wrong_ty, vec![8])).unwrap_err(),
            StatusCode::BadType,
            "TYPE != TYPE_NATIVE_FILE_DESCRIPTOR rejected"
        );

        // (5) hasComm != 0: rsbinder has no comm channel (AOSP would read a second fd).
        let mut comm = ok_body.clone();
        comm[4..8].copy_from_slice(&1i32.to_le_bytes());
        assert_eq!(
            de(&mut v1_reader(&comm, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "hasComm != 0 rejected (AC-11.5)"
        );

        // Well-formed body fails only at the absent in-fd, so the gates are what rejected (1)–(5).
        assert_eq!(
            de(&mut v1_reader(&ok_body, vec![8])).unwrap_err(),
            StatusCode::BadValue,
            "well-formed body reaches the fd lookup (no in-fd installed)"
        );
    }
}

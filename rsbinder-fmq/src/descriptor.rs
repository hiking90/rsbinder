// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! The queue's geometry as libfmq's `MQDescriptor` states it, and the checks
//! an [`AttachPolicy`] applies to a descriptor that arrived from a peer.

use std::os::fd::{AsFd, OwnedFd};

use crate::error::{Error, Result};
use crate::shm;

/// `MQFlavor` — which reader/writer discipline the queue follows.
///
/// The AIDL `MQDescriptor.flags` field carries this value ([`Flavor::flags`]).
/// Only [`SynchronizedReadWrite`](Flavor::SynchronizedReadWrite) can be
/// attached; the unsynchronized flavor is modelled so a descriptor round-trips
/// without loss.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// One reader, one writer; a full queue refuses the write.
    SynchronizedReadWrite,
    /// One writer, any number of readers; a full queue overwrites.
    UnsynchronizedWrite,
}

impl Flavor {
    /// The `MQFlavor` value libfmq stores in `MQDescriptor.flags`.
    pub const fn flags(self) -> u32 {
        match self {
            Flavor::SynchronizedReadWrite => 0x01,
            Flavor::UnsynchronizedWrite => 0x02,
        }
    }

    /// The inverse of [`flags`](Self::flags); `None` for any other value.
    pub const fn from_flags(flags: u32) -> Option<Flavor> {
        match flags {
            0x01 => Some(Flavor::SynchronizedReadWrite),
            0x02 => Some(Flavor::UnsynchronizedWrite),
            _ => None,
        }
    }
}

/// One region of shared memory: `extent` bytes at `offset` into the fd at
/// `fd_index` of the descriptor's [`fds`](Descriptor::fds).
///
/// The AIDL `GrantorDescriptor` uses signed fields; a negative value is
/// invalid there, so the model is unsigned and the AIDL conversion is where
/// the sign check belongs.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Grantor {
    /// Index into [`Descriptor::fds`].
    pub fd_index: u32,
    /// Byte offset into that fd. libfmq requires a multiple of 8.
    pub offset: u32,
    /// Byte length of the region.
    pub extent: u64,
}

/// Everything a peer needs to attach: the fds, the regions inside them, the
/// element size and the flavor. The same information as AIDL
/// `android.hardware.common.fmq.MQDescriptor`, without any notion of how the
/// fds travel.
///
/// The grantor at each index has a fixed role, as in libfmq
/// (`MQDescriptorBase.h`): [`READ_COUNTER`](Self::READ_COUNTER),
/// [`WRITE_COUNTER`](Self::WRITE_COUNTER), [`DATA`](Self::DATA) and, when
/// present, [`EVENT_FLAG_WORD`](Self::EVENT_FLAG_WORD). Any grantor past
/// those four is carried but not interpreted.
#[derive(Debug)]
pub struct Descriptor {
    /// `NativeHandle.fds`.
    pub fds: Vec<OwnedFd>,
    /// `NativeHandle.ints`. libfmq copies them into its `native_handle_t`
    /// and reads none of them; they are kept so a descriptor round-trips.
    pub ints: Vec<i32>,
    /// `MQDescriptor.grantors`.
    pub grantors: Vec<Grantor>,
    /// `MQDescriptor.quantum`: the size in bytes of one element.
    pub quantum: u32,
    /// Decoded `MQDescriptor.flags`.
    pub flavor: Flavor,
}

impl Descriptor {
    /// Grantor index of the reader's 8-byte counter (bytes consumed so far).
    pub const READ_COUNTER: usize = 0;
    /// Grantor index of the writer's 8-byte counter (bytes produced so far).
    pub const WRITE_COUNTER: usize = 1;
    /// Grantor index of the ring itself.
    pub const DATA: usize = 2;
    /// Grantor index of the 4-byte EventFlag word. Optional: a queue made
    /// without one has three grantors and no blocking operations.
    pub const EVENT_FLAG_WORD: usize = 3;

    /// Whether the descriptor carries an EventFlag word.
    pub fn has_event_flag(&self) -> bool {
        self.grantors.len() > Self::EVENT_FLAG_WORD
    }

    /// Elements the ring holds: the data extent divided by the quantum.
    /// `None` when there is no data grantor or the quantum is zero.
    pub fn capacity(&self) -> Option<u64> {
        let data = self.grantors.get(Self::DATA)?;
        if self.quantum == 0 {
            return None;
        }
        Some(data.extent / u64::from(self.quantum))
    }

    /// A copy whose fds are duplicates (`F_DUPFD_CLOEXEC`) of this one's.
    pub fn try_clone(&self) -> Result<Self> {
        let mut fds = Vec::with_capacity(self.fds.len());
        for fd in &self.fds {
            fds.push(fd.try_clone().map_err(io_to_errno)?);
        }
        Ok(Self {
            fds,
            ints: self.ints.clone(),
            grantors: self.grantors.clone(),
            quantum: self.quantum,
            flavor: self.flavor,
        })
    }
}

fn io_to_errno(e: std::io::Error) -> Error {
    Error::Os(crate::error::errno_of(&e))
}

/// What a receiver demands of a descriptor before mapping it. libfmq's own
/// checks (field ranges, alignment, minimum extents, quantum) always run;
/// these are the ones libfmq does not make.
///
/// There is no `Default`: `max_capacity` is a bound only the caller can
/// choose, and a receiver that maps a peer-sized region without one has
/// handed the peer control of its address space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AttachPolicy {
    /// Largest element count the receiver will map.
    pub max_capacity: usize,
    /// Require every fd to be either a memfd with `F_SEAL_SHRINK` or an
    /// ashmem region, so the peer cannot shrink it under a live mapping
    /// (a later access would raise `SIGBUS`). An ashmem region can be
    /// resized until its first `mmap`, so attaching re-reads its size after
    /// mapping and refuses a region that no longer covers the grantors.
    pub require_seal: bool,
    /// Require the EventFlag word, without which no blocking operation
    /// exists.
    pub require_event_flag: bool,
}

/// Grantor offsets are multiples of this (`alignToWordBoundary`, 64 bits).
pub(crate) const ALIGN: u64 = 8;

pub(crate) const fn align_up(x: u64) -> u64 {
    (x + ALIGN - 1) & !(ALIGN - 1)
}

/// `AidlMQDescriptorShimBase.h` single-fd layout: counters, ring, EventFlag word, each 8-aligned.
pub(crate) fn default_layout(data_bytes: u64, event_flag: bool) -> (Vec<Grantor>, u64) {
    let sizes: [u64; 4] = [8, 8, data_bytes, 4];
    let count = if event_flag { 4 } else { 3 };
    let mut grantors = Vec::with_capacity(count);
    let mut offset = 0u64;
    let mut end = 0u64;
    for &size in &sizes[..count] {
        let start = align_up(offset);
        grantors.push(Grantor {
            fd_index: 0,
            offset: start as u32,
            extent: size,
        });
        end = start + size;
        offset = end;
    }
    (grantors, end)
}

/// The regions a validated descriptor maps, by role.
pub(crate) struct Geometry {
    pub read: Grantor,
    pub write: Grantor,
    pub data: Grantor,
    pub event_flag: Option<Grantor>,
    pub capacity: usize,
}

/// What the grantor overlap check compares; a consistency check, not a safety boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Object {
    /// A non-ashmem fd's file: every `dup` of one fd shares `st_dev` and `st_ino`.
    File { dev: u64, ino: u64 },
    /// An ashmem fd, by index: every ashmem fd has the device node's inode.
    Ashmem(usize),
}

impl Object {
    #[allow(clippy::unnecessary_cast)] // `st_dev`/`st_ino` widths differ by target
    fn of(fd: std::os::fd::BorrowedFd<'_>, index: usize) -> Result<Self> {
        if shm::is_ashmem_fd(fd) {
            return Ok(Object::Ashmem(index));
        }
        let st = rustix::fs::fstat(fd)?;
        Ok(Object::File {
            dev: st.st_dev as u64,
            ino: st.st_ino as u64,
        })
    }
}

/// libfmq's `initMemory`/`mapGrantorDescr` checks, the policy's, and `i32::MAX` offsets/extents.
pub(crate) fn validate(
    desc: &Descriptor,
    quantum: usize,
    policy: &AttachPolicy,
) -> Result<Geometry> {
    if desc.flavor != Flavor::SynchronizedReadWrite {
        return Err(Error::BadValue("only the synchronized flavor is supported"));
    }
    if quantum == 0 || desc.quantum as usize != quantum {
        return Err(Error::BadValue("quantum differs from the element size"));
    }
    let n = desc.grantors.len();
    if n <= Descriptor::DATA {
        return Err(Error::BadValue("fewer than three grantors"));
    }
    if policy.require_event_flag && n <= Descriptor::EVENT_FLAG_WORD {
        return Err(Error::BadValue("no EventFlag word"));
    }
    let used = &desc.grantors[..n.min(Descriptor::EVENT_FLAG_WORD + 1)];

    const MIN_EXTENT: [u64; 4] = [8, 8, 1, 4];
    let mut sizes: Vec<Option<(u64, Object)>> = vec![None; desc.fds.len()];
    for (i, g) in used.iter().enumerate() {
        let fd_index = g.fd_index as usize;
        if fd_index >= desc.fds.len() {
            return Err(Error::BadValue("grantor fd index out of range"));
        }
        if u64::from(g.offset) % ALIGN != 0 {
            return Err(Error::BadValue("grantor offset not 8-byte aligned"));
        }
        if g.offset > i32::MAX as u32 || g.extent > i32::MAX as u64 {
            return Err(Error::BadValue("grantor offset or extent exceeds i32"));
        }
        if g.extent < MIN_EXTENT[i] {
            return Err(Error::BadValue(
                "grantor extent below the minimum for its role",
            ));
        }
        let size = match sizes[fd_index] {
            Some((s, _)) => s,
            None => {
                // Seal first: a memfd seal is permanent; ashmem's size is re-checked once mapped.
                let fd = desc.fds[fd_index].as_fd();
                if policy.require_seal && !(shm::shrink_sealed(fd) || shm::is_ashmem_fd(fd)) {
                    return Err(Error::BadValue("fd is neither shrink-sealed nor ashmem"));
                }
                let s = shm::region_size(fd)?;
                sizes[fd_index] = Some((s, Object::of(fd, fd_index)?));
                s
            }
        };
        if u64::from(g.offset) + g.extent > size {
            return Err(Error::BadValue("grantor extends past the end of its fd"));
        }
    }

    let data = used[Descriptor::DATA];
    if data.extent % quantum as u64 != 0 {
        return Err(Error::BadValue("data extent not a multiple of the quantum"));
    }
    // ≤ i32::MAX, so it fits a 32-bit usize.
    let capacity = (data.extent / quantum as u64) as usize;
    if capacity > policy.max_capacity {
        return Err(Error::BadValue(
            "capacity above the attach policy's maximum",
        ));
    }

    // Every used grantor's fd was visited above, so its object is known.
    let object = |g: &Grantor| sizes[g.fd_index as usize].map(|(_, o)| o);
    for (a, ga) in used.iter().enumerate() {
        for gb in &used[a + 1..] {
            if object(ga) != object(gb) {
                continue;
            }
            let (a0, a1) = (u64::from(ga.offset), u64::from(ga.offset) + ga.extent);
            let (b0, b1) = (u64::from(gb.offset), u64::from(gb.offset) + gb.extent);
            if a0 < b1 && b0 < a1 {
                return Err(Error::BadValue("grantor regions overlap"));
            }
        }
    }

    Ok(Geometry {
        read: used[Descriptor::READ_COUNTER],
        write: used[Descriptor::WRITE_COUNTER],
        data,
        event_flag: used.get(Descriptor::EVENT_FLAG_WORD).copied(),
        capacity,
    })
}

/// Ashmem's size is fixed only by its first `mmap`: re-check each grantor's end once mapped.
pub(crate) fn recheck_ashmem_sizes(desc: &Descriptor, geo: &Geometry) -> Result<()> {
    let grantors = [geo.read, geo.write, geo.data]
        .into_iter()
        .chain(geo.event_flag);
    for g in grantors {
        let fd = desc.fds[g.fd_index as usize].as_fd();
        if shm::is_ashmem_fd(fd) && u64::from(g.offset) + g.extent > shm::ashmem_size(fd)? {
            return Err(Error::BadValue("grantor extends past the end of its fd"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_matches_libfmq_offsets() {
        let (g, total) = default_layout(100, true);
        assert_eq!(
            g[0],
            Grantor {
                fd_index: 0,
                offset: 0,
                extent: 8
            }
        );
        assert_eq!(
            g[1],
            Grantor {
                fd_index: 0,
                offset: 8,
                extent: 8
            }
        );
        assert_eq!(
            g[2],
            Grantor {
                fd_index: 0,
                offset: 16,
                extent: 100
            }
        );
        // 16 + 100 = 116 → next multiple of 8 is 120.
        assert_eq!(
            g[3],
            Grantor {
                fd_index: 0,
                offset: 120,
                extent: 4
            }
        );
        assert_eq!(total, 124);

        let (g, total) = default_layout(64, false);
        assert_eq!(g.len(), 3);
        assert_eq!(total, 80);
    }

    #[test]
    fn an_io_error_without_a_valid_errno_maps_to_eio() {
        // linux_raw panics on these and libc keeps them; both backends must give `EIO`.
        for code in [0, -1, 4096, i32::MAX] {
            assert_eq!(
                io_to_errno(std::io::Error::from_raw_os_error(code)),
                Error::Os(rustix::io::Errno::IO),
                "{code}"
            );
        }
        assert_eq!(
            io_to_errno(std::io::Error::other("no code")),
            Error::Os(rustix::io::Errno::IO)
        );
        let emfile = rustix::io::Errno::MFILE.raw_os_error();
        assert_eq!(
            io_to_errno(std::io::Error::from_raw_os_error(emfile)),
            Error::Os(rustix::io::Errno::MFILE)
        );
    }

    #[test]
    fn flavor_flags_round_trip() {
        for f in [Flavor::SynchronizedReadWrite, Flavor::UnsynchronizedWrite] {
            assert_eq!(Flavor::from_flags(f.flags()), Some(f));
        }
        assert_eq!(Flavor::from_flags(0), None);
        assert_eq!(Flavor::from_flags(3), None);
    }
}

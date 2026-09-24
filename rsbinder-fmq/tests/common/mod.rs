// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A `Descriptor` over a unix socket: fds as `SCM_RIGHTS`, the rest as a
//! little-endian record. The C++ peer in `tests/cpp/libfmq_peer.cpp` speaks
//! the same record, so keep the two in step:
//!
//! ```text
//! u32 len            bytes that follow
//! u32 quantum
//! u32 flags          MQFlavor value
//! u32 n_grantors
//! u32 n_ints
//! n_grantors × { u32 fd_index, u32 offset, u64 extent }
//! n_ints × i32
//! ```

#![allow(dead_code)]

use std::io::{IoSlice, IoSliceMut};
use std::mem::MaybeUninit;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use rsbinder_fmq::{Descriptor, Flavor, Grantor};

pub fn send_descriptor(sock: &UnixStream, desc: &Descriptor) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&desc.quantum.to_le_bytes());
    bytes.extend_from_slice(&desc.flavor.flags().to_le_bytes());
    bytes.extend_from_slice(&(desc.grantors.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(desc.ints.len() as u32).to_le_bytes());
    for g in &desc.grantors {
        bytes.extend_from_slice(&g.fd_index.to_le_bytes());
        bytes.extend_from_slice(&g.offset.to_le_bytes());
        bytes.extend_from_slice(&g.extent.to_le_bytes());
    }
    for i in &desc.ints {
        bytes.extend_from_slice(&i.to_le_bytes());
    }
    let fds: Vec<_> = desc.fds.iter().map(|fd| fd.as_fd()).collect();
    let mut space = vec![MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = rustix::net::SendAncillaryBuffer::new(&mut space);
    assert!(control.push(rustix::net::SendAncillaryMessage::ScmRights(&fds)));
    let len = (bytes.len() as u32).to_le_bytes();
    let sent = rustix::net::sendmsg(
        sock,
        &[IoSlice::new(&len), IoSlice::new(&bytes)],
        &mut control,
        rustix::net::SendFlags::empty(),
    )
    .expect("sendmsg");
    assert_eq!(sent, 4 + bytes.len());
}

pub fn recv_descriptor(sock: &UnixStream) -> Descriptor {
    let mut data = vec![0u8; 4096];
    let mut space = vec![MaybeUninit::<u8>::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = rustix::net::RecvAncillaryBuffer::new(&mut space);
    let msg = rustix::net::recvmsg(
        sock,
        &mut [IoSliceMut::new(&mut data)],
        &mut control,
        rustix::net::RecvFlags::empty(),
    )
    .expect("recvmsg");
    let mut fds: Vec<OwnedFd> = Vec::new();
    for m in control.drain() {
        if let rustix::net::RecvAncillaryMessage::ScmRights(received) = m {
            fds.extend(received);
        }
    }
    let data = &data[..msg.bytes];
    let u32_at = |o: usize| u32::from_le_bytes(data[o..o + 4].try_into().unwrap());
    let len = u32_at(0) as usize;
    assert_eq!(len + 4, data.len(), "descriptor record length");
    let quantum = u32_at(4);
    let flavor = Flavor::from_flags(u32_at(8)).expect("flavor");
    let n_grantors = u32_at(12) as usize;
    let n_ints = u32_at(16) as usize;
    let mut o = 20;
    let mut grantors = Vec::with_capacity(n_grantors);
    for _ in 0..n_grantors {
        grantors.push(Grantor {
            fd_index: u32_at(o),
            offset: u32_at(o + 4),
            extent: u64::from_le_bytes(data[o + 8..o + 16].try_into().unwrap()),
        });
        o += 16;
    }
    let mut ints = Vec::with_capacity(n_ints);
    for _ in 0..n_ints {
        ints.push(u32_at(o) as i32);
        o += 4;
    }
    Descriptor {
        fds,
        ints,
        grantors,
        quantum,
        flavor,
    }
}

pub fn sock_path(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("rsbinder-fmq-{}-{tag}.sock", std::process::id()))
}

pub fn connect_with_retry(path: &PathBuf) -> UnixStream {
    let started = Instant::now();
    loop {
        match UnixStream::connect(path) {
            Ok(s) => return s,
            Err(_) if started.elapsed() < Duration::from_secs(10) => {
                std::thread::sleep(Duration::from_millis(5))
            }
            Err(e) => panic!("connect {}: {e}", path.display()),
        }
    }
}

pub fn spawn(cmd: &mut Command) -> Child {
    cmd.stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn peer")
}

/// The peer's stdout once it exits; killed and failed if it does not within
/// the deadline (a wait that never woke).
pub fn finish(mut child: Child) -> String {
    use std::io::Read;
    let started = Instant::now();
    let mut stdout = child.stdout.take().expect("piped stdout");
    let reader = std::thread::spawn(move || {
        let mut s = String::new();
        stdout.read_to_string(&mut s).expect("read peer stdout");
        s
    });
    loop {
        match child.try_wait().expect("try_wait") {
            Some(status) => {
                let out = reader.join().expect("stdout reader");
                assert!(status.success(), "peer failed: {status}\n{out}");
                return out;
            }
            None if started.elapsed() > Duration::from_secs(30) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("peer did not finish; a wait never woke");
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

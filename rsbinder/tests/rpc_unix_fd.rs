// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! A Unix socket reached by descriptors: a server on a socket another party bound
//! (`RpcServer::setup_unix_server_from_bound_fd`), and a client that connects through an `O_PATH`
//! descriptor of the socket's file (`RpcClientConfig::unix_fd`), the file unlinked or not.

#![cfg(all(feature = "rpc", any(target_os = "linux", target_os = "android")))]

use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rsbinder::rpc::{
    FileDescriptorTransportMode as FdMode, PeerIdentity, RpcClientConfig, RpcServer,
    RpcServerHandle, RpcSession,
};
use rsbinder::{
    Binder, Interface, Parcel, Remotable, Result, SIBinder, StatusCode, TransactionCode,
};
use rustix::fs::{Mode, OFlags};
use rustix::net::{AddressFamily, SocketAddrUnix, SocketFlags, SocketType};

const WIRE: u32 = 2;

struct Root;

impl Remotable for Root {
    fn descriptor() -> &'static str {
        "rsbinder.test.IUnixFd"
    }
    fn on_transact(&self, _: TransactionCode, _: &mut Parcel, _: &mut Parcel) -> Result<()> {
        Err(StatusCode::UnknownTransaction)
    }
    fn on_dump(&self, _: &mut dyn std::io::Write, _: &[String]) -> Result<()> {
        Ok(())
    }
}

fn root() -> SIBinder {
    Binder::new(Root).as_binder()
}

fn path(tag: &str) -> PathBuf {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let serial = NEXT.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id();
    std::env::temp_dir().join(format!("rsb_unix_fd_{tag}_{pid}_{serial}.sock"))
}

/// A bound Unix stream socket, and its file: the socket with no `listen`.
fn bound(tag: &str) -> (OwnedFd, PathBuf) {
    let path = path(tag);
    let socket = rustix::net::socket_with(
        AddressFamily::UNIX,
        SocketType::STREAM,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::bind(&socket, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    (socket, path)
}

fn path_fd(path: &PathBuf) -> OwnedFd {
    rustix::fs::open(path, OFlags::PATH | OFlags::CLOEXEC, Mode::empty()).unwrap()
}

/// A socket bound and listening through a descriptor alone, as a hub hands one out.
fn unlinked(tag: &str) -> (Arc<RpcServer>, RpcServerHandle, OwnedFd) {
    let (socket, path) = bound(tag);
    let address = path_fd(&path);
    std::fs::remove_file(&path).unwrap();
    let (server, handle) = serve(socket);
    (server, handle, address)
}

fn serve(socket: OwnedFd) -> (Arc<RpcServer>, RpcServerHandle) {
    let server = RpcServer::setup_unix_server_from_bound_fd(socket).unwrap();
    server.set_android13plus(WIRE);
    server.set_supported_fd_modes(&[FdMode::Unix]);
    server.set_root(root()).unwrap();
    let handle = server.run_background().unwrap();
    (server, handle)
}

fn connect(address: OwnedFd) -> Result<RpcSession> {
    let config = RpcClientConfig::unix_fd(address, WIRE).fd_mode(FdMode::Unix);
    RpcSession::setup_client_android13plus_with_config(config.incoming_connections(1))
}

fn is_open(fd: RawFd) -> bool {
    std::fs::read_link(format!("/proc/self/fd/{fd}")).is_ok()
}

#[test]
fn a_client_reaches_a_socket_with_no_name_left_through_its_descriptor() {
    let (server, handle, address) = unlinked("unlinked");
    assert_eq!(server.path(), None);
    let session = connect(address).unwrap();
    session.get_root().unwrap().ping_binder().unwrap();
    assert!(session.caps().contains(
        rsbinder::TransportCaps::FD_PASSING
            | rsbinder::TransportCaps::SAME_HOST
            | rsbinder::TransportCaps::CALLBACKS
    ));
    session.close_session();
    handle.stop_and_join().unwrap();
}

#[test]
fn every_connection_of_a_session_goes_through_the_one_descriptor() {
    let (_server, handle, address) = unlinked("redial");
    let number = address.as_raw_fd();
    let config = RpcClientConfig::unix_fd(address, WIRE)
        .fd_mode(FdMode::Unix)
        .incoming_connections(1);
    let session = RpcSession::setup_client_android13plus_with_config(config).unwrap();
    assert!(is_open(number), "the session owns the descriptor");
    session.add_incoming_connection().unwrap();
    session.get_root().unwrap().ping_binder().unwrap();
    session.close_session();
    drop(session);
    let deadline = Instant::now() + Duration::from_secs(10);
    while is_open(number) {
        assert!(
            Instant::now() < deadline,
            "the session keeps its descriptor"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    handle.stop_and_join().unwrap();
}

#[test]
fn a_client_learns_who_listens_from_its_session() {
    let (_server, handle, address) = unlinked("peer");
    let session = connect(address).unwrap();
    let PeerIdentity::Local { uid, pid, .. } = session.peer_identity() else {
        panic!("{:?}", session.peer_identity());
    };
    assert_eq!(uid, rustix::process::geteuid().as_raw());
    assert_eq!(pid, std::process::id() as i32);
    session.close_session();
    handle.stop_and_join().unwrap();
}

#[test]
fn a_descriptor_of_a_socket_nobody_listens_on_is_refused() {
    let (socket, path) = bound("refused");
    let address = path_fd(&path);
    let early = path_fd(&path);
    std::fs::remove_file(&path).unwrap();
    assert_eq!(
        connect(early).err(),
        Some(StatusCode::Errno(-libc::ECONNREFUSED))
    );
    let (_server, handle) = serve(socket);
    let session = connect(address).unwrap();
    session.close_session();
    handle.stop_and_join().unwrap();
}

#[test]
fn a_descriptor_keeps_refusing_once_its_listener_closed() {
    let (socket, path) = bound("closed");
    let address = path_fd(&path);
    std::fs::remove_file(&path).unwrap();
    let (server, handle) = serve(socket);
    handle.stop_and_join().unwrap();
    drop(server);
    assert_eq!(
        connect(address).err(),
        Some(StatusCode::Errno(-libc::ECONNREFUSED))
    );
}

#[test]
fn a_server_on_a_bound_descriptor_leaves_the_file_alone_when_dropped() {
    let (socket, path) = bound("file");
    let (server, handle) = serve(socket);
    assert_eq!(server.path(), None);
    handle.stop_and_join().unwrap();
    drop(server);
    assert!(path.exists());
    std::fs::remove_file(&path).unwrap();
}

#[test]
fn a_listening_socket_is_taken_too() {
    let (socket, path) = bound("listening");
    rustix::net::listen(&socket, 1).unwrap();
    let address = path_fd(&path);
    std::fs::remove_file(&path).unwrap();
    let (_server, handle) = serve(socket);
    let session = connect(address).unwrap();
    session.close_session();
    handle.stop_and_join().unwrap();
}

#[test]
fn anything_but_a_bound_unix_stream_socket_is_refused() {
    let refused = |fd: OwnedFd| RpcServer::setup_unix_server_from_bound_fd(fd).err();
    let socket =
        |family, kind| rustix::net::socket_with(family, kind, SocketFlags::CLOEXEC, None).unwrap();

    let (read, write) = rustix::pipe::pipe().unwrap();
    assert_eq!(refused(read), Some(StatusCode::BadValue), "a pipe");
    drop(write);

    let unbound = socket(AddressFamily::UNIX, SocketType::STREAM);
    assert_eq!(refused(unbound), Some(StatusCode::BadValue), "not bound");

    let tcp = socket(AddressFamily::INET, SocketType::STREAM);
    assert_eq!(refused(tcp), Some(StatusCode::BadValue), "another family");

    let datagram = socket(AddressFamily::UNIX, SocketType::DGRAM);
    let path = path("dgram");
    rustix::net::bind(&datagram, &SocketAddrUnix::new(&path).unwrap()).unwrap();
    assert_eq!(
        refused(datagram),
        Some(StatusCode::BadValue),
        "not a stream"
    );
    std::fs::remove_file(&path).unwrap();
}

/// The socket file's mode bits still judge a connect through a descriptor (af_unix.c).
#[test]
fn the_files_mode_judges_a_connect_through_a_descriptor() {
    let (socket, path) = bound("mode");
    let address = path_fd(&path);
    let writable = path_fd(&path);
    let (_server, handle) = serve(socket);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o400)).unwrap();
    if !rustix::process::geteuid().is_root() {
        assert_eq!(
            connect(address).err(),
            Some(StatusCode::Errno(-libc::EACCES))
        );
    }
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let session = connect(writable).unwrap();
    session.close_session();
    std::fs::remove_file(&path).unwrap();
    handle.stop_and_join().unwrap();
}

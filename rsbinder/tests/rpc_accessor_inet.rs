// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 17 R-23: with `rpc-tcp-debug` compiled in, an `IAccessor` that hands
//! back an `AF_INET` socket is refused unless the process opted in with
//! `rpc::set_accessor_plaintext_inet(true)`.
//!
//! Its own test binary because the switch is process-wide: a test that flips
//! it would change what every other accessor test in the same process sees.
//! One test, so the order of the flips is the order written.
//!
//! # Mutation gates
//!
//! - `an_inet_accessor_socket_needs_the_opt_in`: removing the
//!   `refuse_plaintext_inet` call from `connect_accessor` lets the first
//!   resolve reach the handshake, which fails with `DeadObject` against the
//!   closing listener instead of `BadType`. Reading the switch inside
//!   `from_preconnected_fd` instead fails the last assertion.

#![cfg(all(feature = "rpc", feature = "android_16", feature = "rpc-tcp-debug"))]

use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;

use rsbinder::hub::android_16::{
    try_resolve_accessor, BnAccessor, IAccessor, ERROR_FAILED_TO_CONNECT_TO_SOCKET,
};
use rsbinder::rpc::{accessor_plaintext_inet, set_accessor_plaintext_inet, RpcSession};
use rsbinder::{Interface, ParcelFileDescriptor, SIBinder, Status, StatusCode, Strong};

const NAME: &str = "test.inet";

/// An accessor whose every connection is a TCP socket to `addr`.
struct InetAccessor {
    addr: std::net::SocketAddr,
}

impl Interface for InetAccessor {}
impl IAccessor for InetAccessor {
    fn r#addConnection(&self) -> rsbinder::BinderResult<ParcelFileDescriptor> {
        let stream = TcpStream::connect(self.addr).map_err(|e| {
            Status::new_service_specific_error(
                ERROR_FAILED_TO_CONNECT_TO_SOCKET,
                Some(format!("connect failed: {e}")),
            )
        })?;
        Ok(ParcelFileDescriptor::new(stream))
    }

    fn r#getInstanceName(&self) -> rsbinder::BinderResult<String> {
        Ok(NAME.to_string())
    }
}

/// A listener that accepts and closes every connection: no RPC server behind it.
fn closing_listener() -> (
    std::net::SocketAddr,
    Arc<AtomicBool>,
    thread::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.set_nonblocking(true).expect("nonblocking");
    let addr = listener.local_addr().expect("local_addr");
    let stop = Arc::new(AtomicBool::new(false));
    let accepting = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((s, _)) => drop(s),
                    Err(_) => thread::sleep(std::time::Duration::from_millis(5)),
                }
            }
        })
    };
    (addr, stop, accepting)
}

#[test]
fn an_inet_accessor_socket_needs_the_opt_in() {
    let (addr, stop, accepting) = closing_listener();
    let strong: Strong<dyn IAccessor> = BnAccessor::new_binder(InetAccessor { addr });
    let accessor: SIBinder = Interface::as_binder(&*strong);

    assert!(!accessor_plaintext_inet(), "off by default");
    let err = try_resolve_accessor(NAME, accessor.clone()).expect_err("refused");
    assert_eq!(
        err.transaction_error(),
        StatusCode::BadType,
        "an inet socket from an accessor is refused while the switch is off"
    );

    assert!(
        !set_accessor_plaintext_inet(true),
        "returns the value it replaces"
    );
    assert!(accessor_plaintext_inet());
    // Past the gate: the handshake runs and fails against a listener that closes at once.
    let err = try_resolve_accessor(NAME, accessor.clone()).expect_err("no server");
    assert_ne!(
        err.transaction_error(),
        StatusCode::BadType,
        "with the switch on, the inet socket reaches the handshake"
    );

    assert!(set_accessor_plaintext_inet(false));
    // A direct `from_preconnected_fd` is the caller's own choice and never reads the switch.
    let fd = std::os::fd::OwnedFd::from(TcpStream::connect(addr).expect("connect"));
    let direct = RpcSession::from_preconnected_fd(fd, 2).err();
    assert!(
        direct.is_some_and(|e| e != StatusCode::BadType),
        "from_preconnected_fd must not apply the accessor switch: {direct:?}"
    );

    stop.store(true, Ordering::SeqCst);
    accepting.join().unwrap();
}

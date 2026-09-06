// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! End-to-end round trip for the argument shapes the AOSP fixture corpus never
//! uses (non-nullable `out` binder, `@nullable` primitive arrays, non-nullable
//! `inout` binder array), so that their codegen is run, not only type-checked.
//!
//! Driven over the RPC transport so the same test runs on Linux, macOS and an
//! Android device without a kernel binder node. The generated server stub and
//! `Bp*` proxy are both reused unmodified — the point is the wire.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::thread;

use rsbinder::rpc::transport::{MemTransport, UnixTransport};
use rsbinder::rpc::{AddressSpace, RpcSession, RpcTransport};
use rsbinder::{ExceptionCode, FromIBinder, Interface, SIBinder};

include!(concat!(env!("OUT_DIR"), "/codegen_shapes.rs"));

use shapes::ICodegenShapes::{BnCodegenShapes, ICodegenShapes};

struct ShapesSvc;
impl Interface for ShapesSvc {}

impl ICodegenShapes for ShapesSvc {
    fn r#takeOutBinder(
        &self,
        src: &SIBinder,
        fill: bool,
        dst: &mut Option<SIBinder>,
    ) -> rsbinder::status::Result<()> {
        if fill {
            *dst = Some(src.clone());
        }
        Ok(())
    }

    fn r#roundNullableVec(&self, v: &mut Option<Vec<i32>>) -> rsbinder::status::Result<()> {
        if let Some(v) = v.as_mut() {
            v.push(99);
        }
        Ok(())
    }

    fn r#roundNullableFixed(
        &self,
        v: Option<&[i32; 3]>,
        r: &mut Option<[i32; 3]>,
    ) -> rsbinder::status::Result<()> {
        *r = v.map(|v| [v[2], v[1], v[0]]);
        Ok(())
    }

    fn r#roundInoutBinders(&self, v: &mut Vec<SIBinder>) -> rsbinder::status::Result<()> {
        v.reverse();
        Ok(())
    }
}

fn root() -> SIBinder {
    BnCodegenShapes::new_binder(ShapesSvc).as_binder()
}

fn run(server_t: Box<dyn RpcTransport>, client_t: Box<dyn RpcTransport>) {
    let server = RpcSession::new(server_t, AddressSpace::Acceptor).expect("RpcSession::new");
    server.set_root(root()).expect("set_root");
    let server_for_thread = server.clone();
    let handle = thread::spawn(move || {
        let _ = server_for_thread.serve_blocking();
    });

    {
        let client = RpcSession::new(client_t, AddressSpace::Initiator).expect("RpcSession::new");
        let sib = client.get_root().expect("get_root");
        let shapes = <dyn ICodegenShapes as FromIBinder>::try_from(sib.clone())
            .expect("generated BpCodegenShapes resolves from an RPC binder");

        // A non-nullable `out` binder the service fills comes back.
        let mut dst = None;
        shapes
            .r#takeOutBinder(&sib, true, &mut dst)
            .expect("a filled out-binder round-trips");
        assert!(dst.is_some(), "the out binder must come back");

        // Left unset it is UNEXPECTED_NULL from the server, not a null on the
        // wire that the client silently accepts.
        let mut dst = Some(sib.clone());
        let err = shapes
            .r#takeOutBinder(&sib, false, &mut dst)
            .expect_err("an unset non-nullable out binder must fail the transaction");
        assert_eq!(
            err.exception_code(),
            ExceptionCode::NullPointer,
            "got: {err:?}"
        );

        // `@nullable int[]` carries no per-element null marker: a bare
        // `Vec<i32>` on both sides.
        let mut v = Some(vec![1, 2, 3]);
        shapes
            .r#roundNullableVec(&mut v)
            .expect("inout nullable vec");
        assert_eq!(v, Some(vec![1, 2, 3, 99]));
        let mut v: Option<Vec<i32>> = None;
        shapes
            .r#roundNullableVec(&mut v)
            .expect("a null inout vec stays null");
        assert_eq!(v, None);

        // Same rule for a fixed-size `@nullable` array.
        let mut r = None;
        shapes
            .r#roundNullableFixed(Some(&[4, 5, 6]), &mut r)
            .expect("nullable fixed array");
        assert_eq!(r, Some([6, 5, 4]));
        let mut r = Some([0, 0, 0]);
        shapes
            .r#roundNullableFixed(None, &mut r)
            .expect("a null fixed array stays null");
        assert_eq!(r, None);

        // A non-nullable `inout` binder array is a bare `Vec<SIBinder>`.
        let other = BnCodegenShapes::new_binder(ShapesSvc).as_binder();
        let mut v = vec![sib.clone(), other.clone()];
        shapes
            .r#roundInoutBinders(&mut v)
            .expect("inout binder array");
        // The service reversed it: the callee's mutation, not the input, comes back.
        assert_eq!(v, vec![other, sib.clone()], "the reversed array comes back");
    }

    handle.join().expect("server thread");
}

#[test]
fn codegen_shapes_over_mem() {
    let (a, b) = MemTransport::pair();
    run(Box::new(a), Box::new(b));
}

#[test]
fn codegen_shapes_over_unix_socketpair() {
    let (a, b) = UnixTransport::pair().expect("socketpair");
    run(Box::new(a), Box::new(b));
}

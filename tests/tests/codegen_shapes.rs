// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Runs, not only type-checks, codegen for shapes the AOSP fixture corpus never uses.
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

use shapes::ICodegenShapes::{BnCodegenShapes, FixedTagged, ICodegenShapes};

struct ShapesSvc;
impl Interface for ShapesSvc {}

impl ICodegenShapes for ShapesSvc {
    fn r#takeOutBinders(
        &self,
        src: &SIBinder,
        fill: bool,
        dst: &mut Vec<Option<SIBinder>>,
    ) -> rsbinder::BinderResult<()> {
        if fill {
            dst.fill(Some(src.clone()));
        }
        Ok(())
    }

    fn r#roundNullableVec(&self, v: &mut Option<Vec<i32>>) -> rsbinder::BinderResult<()> {
        if let Some(v) = v.as_mut() {
            v.push(99);
        }
        Ok(())
    }

    fn r#roundNullableFixed(
        &self,
        v: Option<&[i32; 3]>,
        r: &mut Option<[i32; 3]>,
    ) -> rsbinder::BinderResult<()> {
        *r = v.map(|v| [v[2], v[1], v[0]]);
        Ok(())
    }

    fn r#roundInoutBinders(&self, v: &mut Vec<SIBinder>) -> rsbinder::BinderResult<()> {
        v.reverse();
        Ok(())
    }

    fn r#reverseTags(
        &self,
        tags: &[FixedTagged::Tag],
    ) -> rsbinder::BinderResult<Vec<FixedTagged::Tag>> {
        Ok(tags.iter().rev().copied().collect())
    }

    fn r#failUnexpectedNull(&self) -> rsbinder::BinderResult<()> {
        Err(rsbinder::StatusCode::UnexpectedNull.into())
    }

    fn r#failDeadObject(&self) -> rsbinder::BinderResult<()> {
        Err(rsbinder::StatusCode::DeadObject.into())
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

        // A caller-sized, non-nullable `out` binder array comes back filled.
        let mut dst = vec![None, None];
        shapes
            .r#takeOutBinders(&sib, true, &mut dst)
            .expect("a filled out-binder array round-trips");
        assert_eq!(dst, vec![Some(sib.clone()), Some(sib.clone())]);

        // Unset elements come back null, as AOSP's Rust backend (it guards only fd arrays).
        let mut dst = vec![Some(sib.clone())];
        shapes
            .r#takeOutBinders(&sib, false, &mut dst)
            .expect("an unset out-binder element is a legal null binder");
        assert_eq!(dst, vec![None]);

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

        let tags = [
            FixedTagged::Tag::a,
            FixedTagged::Tag::b,
            FixedTagged::Tag::b,
        ];
        let got = shapes
            .r#reverseTags(&tags)
            .expect("a FixedSize union Tag[]");
        assert_eq!(
            got,
            [
                FixedTagged::Tag::b,
                FixedTagged::Tag::b,
                FixedTagged::Tag::a
            ]
        );

        let err = shapes
            .r#failUnexpectedNull()
            .expect_err("the service fails it");
        assert_eq!(
            err.exception_code(),
            ExceptionCode::TransactionFailed,
            "got: {err:?}"
        );
        assert_eq!(
            err.transaction_error(),
            rsbinder::StatusCode::UnexpectedNull,
            "got: {err:?}"
        );

        let err = shapes.r#failDeadObject().expect_err("the service fails it");
        assert_eq!(
            err.transaction_error(),
            rsbinder::StatusCode::FailedTransaction,
            "a handler's DeadObject is replied as FailedTransaction; got: {err:?}"
        );
        shapes
            .r#reverseTags(&[])
            .expect("the session survives a handler's DeadObject");
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

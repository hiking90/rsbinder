// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-16 Phase D — cross-transport service facade.
//!
//! The whole point of the facade is that **registration and lookup code is
//! written once** and the transport is picked by construction. This test
//! proves it: a single generic `register_all<R: Registry>` and a single
//! generic `talk<B: Broker>` are driven over the RPC transport via
//! `service::rpc::{Host, Broker}`. The same generic functions would accept
//! `service::kernel::{Host, Broker}` (exercised on REMOTE_LINUX, where
//! `/dev/binder` exists).
//!
//! Separate test binary, `#![cfg(feature = "rpc")]`.

#![cfg(feature = "rpc")]
#![allow(non_snake_case)]

use std::path::PathBuf;

use rsbinder::service::{Broker, Registry};
use rsbinder::{Interface, SIBinder, Strong};

include!(concat!(env!("OUT_DIR"), "/rpc_smoke.rs"));

use rpcsmoke::IRpcSmoke::{BnRpcSmoke, IRpcSmoke};

struct SmokeSvc;
impl Interface for SmokeSvc {}
impl IRpcSmoke for SmokeSvc {
    fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        Ok(s.to_string())
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

/// A service that prefixes its own `tag` onto every echo, so a caller can
/// tell *which* registered binder answered. Used by
/// `facade_incremental_services_all_resolve` to verify the directory maps
/// each name to its own binder, not merely that some binder answers.
struct TaggedSvc {
    tag: String,
}
impl Interface for TaggedSvc {}
impl IRpcSmoke for TaggedSvc {
    fn r#echo(&self, s: &str) -> rsbinder::status::Result<String> {
        Ok(format!("{}:{}", self.tag, s))
    }
    fn r#add(&self, a: i32, b: i32) -> rsbinder::status::Result<i32> {
        Ok(a + b)
    }
    fn r#ping(&self) -> rsbinder::status::Result<()> {
        Ok(())
    }
}

// ---- the transport-agnostic code: written ONCE, generic over the trait --

fn register_all<R: Registry>(reg: &R, binder: SIBinder) -> rsbinder::Result<()> {
    reg.add_service("smoke", binder)
}

fn talk<B: Broker>(broker: &B) -> rsbinder::Result<()> {
    let smoke: Strong<dyn IRpcSmoke> = broker.get_interface("smoke")?;
    assert_eq!(smoke.r#echo("facade").unwrap(), "facade");
    assert_eq!(smoke.r#add(40, 2).unwrap(), 42);
    smoke.r#ping().unwrap();
    Ok(())
}

fn unique_socket_path(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("rsb_facade_{}_{}.sock", tag, std::process::id()));
    p
}

#[test]
fn facade_register_and_talk_over_rpc_unix() {
    use rsbinder::service::rpc::{Broker as RpcBroker, Host as RpcHost};

    let path = unique_socket_path("rpc");

    // Server: build a host, register generically, serve in the background.
    let host = RpcHost::unix(&path).expect("RpcHost::unix");
    let svc = BnRpcSmoke::new_binder(SmokeSvc).as_binder();
    register_all(&host, svc).expect("register_all over RpcHost");
    let _bg = host.serve_background();

    // Client: build a broker, look up + call generically.
    let broker = RpcBroker::unix(&path).expect("RpcBroker::unix");
    talk(&broker).expect("talk over RpcBroker");

    // The host's RPC-only powers stay reachable on the concrete type
    // (not faked onto the shared trait).
    host.server().shutdown();
    let _ = std::fs::remove_file(&path);
}

#[test]
fn facade_lookup_missing_is_name_not_found() {
    use rsbinder::service::rpc::{Broker as RpcBroker, Host as RpcHost};

    let path = unique_socket_path("missing");
    let host = RpcHost::unix(&path).expect("RpcHost::unix");
    // Register one service so the directory root exists, then look up a
    // different name.
    register_all(&host, BnRpcSmoke::new_binder(SmokeSvc).as_binder()).unwrap();
    let _bg = host.serve_background();

    let broker = RpcBroker::unix(&path).expect("RpcBroker::unix");
    let missing = broker.lookup("does-not-exist");
    assert!(
        missing.is_err(),
        "looking up an unregistered name must be an Err, got {missing:?}"
    );

    host.server().shutdown();
    let _ = std::fs::remove_file(&path);
}

/// Incremental registration: several `add_service` calls — including one
/// made *after* the server is already serving — each resolve to **their
/// own** binder. Locks in the O(1) shared-map `add_service` (the directory
/// is built once and reads the live map): a service added later is seen
/// without a per-call rebuild/root-swap, and each name maps to its own
/// binder (the `TaggedSvc` prefix would expose a name→binder mismap).
#[test]
fn facade_incremental_services_all_resolve() {
    use rsbinder::service::rpc::{Broker as RpcBroker, Host as RpcHost};

    let path = unique_socket_path("incremental");
    let host = RpcHost::unix(&path).expect("RpcHost::unix");

    let tagged = |tag: &str| BnRpcSmoke::new_binder(TaggedSvc { tag: tag.into() }).as_binder();
    // Two services registered before serving, one after — separate calls.
    host.add_service("first", tagged("first")).unwrap();
    host.add_service("second", tagged("second")).unwrap();
    let _bg = host.serve_background();
    host.add_service("third", tagged("third")).unwrap();

    let broker = RpcBroker::unix(&path).expect("RpcBroker::unix");
    for name in ["first", "second", "third"] {
        let svc: Strong<dyn IRpcSmoke> = broker
            .get_interface(name)
            .unwrap_or_else(|e| panic!("{name} must resolve, got {e:?}"));
        // `TaggedSvc` prefixes its own name, so this fails if the directory
        // returned a different service's binder for this name.
        assert_eq!(svc.r#echo("x").unwrap(), format!("{name}:x"));
    }

    host.server().shutdown();
    let _ = std::fs::remove_file(&path);
}

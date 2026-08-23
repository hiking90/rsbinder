// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-19 — an interface declared with `#[rsbinder::interface]` really
//! serves and really answers, over a socket rather than in-process, so the
//! generated `on_transact` / `Bp` halves are both exercised.
//!
//! The byte-identity of this generated code against the `.aidl` path is
//! pinned separately, by the golden tests inside `rsbinder-macros`.

#![cfg(all(feature = "macros", feature = "rpc"))]
#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rsbinder::{interface, BinderResult, Interface, Strong};

#[interface(descriptor = "rsbinder.test.IMacroEcho")]
pub trait IMacroEcho {
    fn echo(&self, msg: &str) -> BinderResult<String>;
    fn add(&self, a: i32, b: i32) -> BinderResult<i32>;
    /// Out parameter: the server sizes and fills the caller's vector.
    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()>;
    /// Inout: sent, mutated, read back.
    fn bump(&self, #[inout] values: &mut Vec<i32>) -> BinderResult<()>;
    fn maybe(&self, msg: Option<&str>) -> BinderResult<Option<String>>;
    fn register(&self, cb: &Strong<dyn IMacroSink>) -> BinderResult<()>;
    #[oneway]
    fn ping(&self) -> BinderResult<()>;
}

#[interface(descriptor = "rsbinder.test.IMacroSink")]
pub trait IMacroSink {
    fn hit(&self, tag: &str) -> BinderResult<()>;
}

/// A self-referencing interface: the callback is the same type as the
/// callee. `rsbinder-aidl` used to render this shape as `Strong<dyn Box<I>>`,
/// which did not compile — this test exists so that stays fixed on the macro
/// path too, since `syn` parsing alone would not have caught it.
#[interface(descriptor = "rsbinder.test.IMacroChain")]
pub trait IMacroChain {
    fn relay(&self, next: &Strong<dyn IMacroChain>, msg: &str) -> BinderResult<String>;
    fn name(&self) -> BinderResult<String>;
}

struct Echo {
    pinged: Arc<Mutex<u32>>,
}
impl Interface for Echo {}
impl IMacroEcho for Echo {
    fn echo(&self, msg: &str) -> BinderResult<String> {
        Ok(format!("echo:{msg}"))
    }
    fn add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()> {
        for (i, slot) in values.iter_mut().enumerate() {
            *slot = i as i32 * 10;
        }
        Ok(())
    }
    fn bump(&self, values: &mut Vec<i32>) -> BinderResult<()> {
        for v in values.iter_mut() {
            *v += 1;
        }
        Ok(())
    }
    fn maybe(&self, msg: Option<&str>) -> BinderResult<Option<String>> {
        Ok(msg.map(|m| m.to_uppercase()))
    }
    fn register(&self, cb: &Strong<dyn IMacroSink>) -> BinderResult<()> {
        cb.hit("from-server")
    }
    fn ping(&self) -> BinderResult<()> {
        *self.pinged.lock().unwrap() += 1;
        Ok(())
    }
}

struct Sink {
    seen: Arc<Mutex<Vec<String>>>,
}
impl Interface for Sink {}
impl IMacroSink for Sink {
    fn hit(&self, tag: &str) -> BinderResult<()> {
        self.seen.lock().unwrap().push(tag.to_string());
        Ok(())
    }
}

struct Chain {
    tag: String,
}
impl Interface for Chain {}
impl IMacroChain for Chain {
    fn relay(&self, next: &Strong<dyn IMacroChain>, msg: &str) -> BinderResult<String> {
        Ok(format!("{}>{}:{msg}", self.tag, next.name()?))
    }
    fn name(&self) -> BinderResult<String> {
        Ok(self.tag.clone())
    }
}

struct SockPath(PathBuf);
impl SockPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("rsb_macro_{}_{}.sock", tag, std::process::id()));
        let _ = std::fs::remove_file(&p);
        SockPath(p)
    }
    fn uri(&self, frag: &str) -> String {
        format!("unix://{}{frag}", self.0.display())
    }
}
impl Drop for SockPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn macro_interface_round_trips_over_rpc() {
    let sock = SockPath::new("echo");
    let pinged = Arc::new(Mutex::new(0));
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add(
            "echo",
            BnMacroEcho::new_binder(Echo {
                pinged: pinged.clone(),
            }),
        )
        .expect("add")
        .spawn()
        .expect("spawn");

    let echo: Strong<dyn IMacroEcho> = rsbinder::connect(&sock.uri("#echo")).expect("connect");

    assert_eq!(echo.echo("hi").unwrap(), "echo:hi");
    assert_eq!(echo.add(40, 2).unwrap(), 42);

    // Out: the length travels on the request, the contents on the reply.
    let mut out = vec![0i32; 4];
    echo.fill(&mut out).unwrap();
    assert_eq!(out, vec![0, 10, 20, 30]);

    // Inout: contents travel both ways.
    let mut inout = vec![1i32, 2, 3];
    echo.bump(&mut inout).unwrap();
    assert_eq!(inout, vec![2, 3, 4]);

    assert_eq!(echo.maybe(Some("shout")).unwrap().as_deref(), Some("SHOUT"));
    assert_eq!(echo.maybe(None).unwrap(), None);

    // A oneway call has no reply to wait on, so pair it with a twoway call
    // to know the server has drained it.
    echo.ping().unwrap();
    echo.echo("barrier").unwrap();
    assert_eq!(*pinged.lock().unwrap(), 1);
}

#[test]
fn macro_interface_carries_a_callback_binder() {
    let sock = SockPath::new("cb");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add(
            "echo",
            BnMacroEcho::new_binder(Echo {
                pinged: Arc::new(Mutex::new(0)),
            }),
        )
        .expect("add")
        .spawn()
        .expect("spawn");

    let echo: Strong<dyn IMacroEcho> = rsbinder::connect(&sock.uri("#echo")).expect("connect");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = BnMacroSink::new_binder(Sink { seen: seen.clone() });

    echo.register(&sink).expect("register");
    assert_eq!(seen.lock().unwrap().as_slice(), ["from-server"]);
}

/// The descriptor is what goes on the wire, so a mismatch must be caught at
/// the cast rather than producing a proxy that fails later.
#[test]
fn macro_interface_descriptor_is_the_attribute_value() {
    assert_eq!(
        <Echo as IMacroEcho>::descriptor(),
        "rsbinder.test.IMacroEcho"
    );
    assert_eq!(
        <Sink as IMacroSink>::descriptor(),
        "rsbinder.test.IMacroSink"
    );
}

#[test]
fn macro_interface_can_reference_itself() {
    let sock = SockPath::new("chain");
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add(
            "chain",
            BnMacroChain::new_binder(Chain {
                tag: "server".into(),
            }),
        )
        .expect("add")
        .spawn()
        .expect("spawn");

    let chain: Strong<dyn IMacroChain> = rsbinder::connect(&sock.uri("#chain")).expect("connect");
    let local = BnMacroChain::new_binder(Chain {
        tag: "local".into(),
    });

    // The server calls back into the binder we handed it, of its own type.
    assert_eq!(chain.relay(&local, "hi").unwrap(), "server>local:hi");
}

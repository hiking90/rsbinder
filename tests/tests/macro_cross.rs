// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Plan 2-19 P3 — `#[rsbinder::interface]` and `.aidl` on opposite ends of
//! the same socket.
//!
//! The golden tests inside `rsbinder-macros` prove the two front-ends emit
//! the same *text*. This proves the resulting code actually interoperates:
//! same descriptor, same transaction numbering, same parcel layout — in both
//! directions, with a macro-declared parcelable and enum crossing between
//! `.aidl`-generated types and back.

#![cfg(all(feature = "rpc", feature = "macros"))]
#![allow(non_snake_case)]

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use rsbinder::{interface, BinderEnum, BinderResult, Interface, Parcelable, Strong};

include!(concat!(env!("OUT_DIR"), "/macro_cross.rs"));

use macrocross::CrossCfg::CrossCfg as AidlCfg;
use macrocross::CrossMode::CrossMode as AidlMode;
use macrocross::IMacroCross::{BnMacroCross as AidlBn, IMacroCross as AidlIface};

// Method order is the transaction numbering: it must match the `.aidl`.

#[derive(Parcelable, Default, Debug, Clone, PartialEq)]
#[parcelable(descriptor = "macrocross.CrossCfg")]
pub struct MacroCfg {
    pub name: String,
    pub retries: i32,
    pub extra: Option<Vec<u8>>,
}

#[derive(BinderEnum, Default, Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum MacroMode {
    #[default]
    FAST = 0,
    SAFE = 1,
}

#[interface(descriptor = "macrocross.IMacroCross")]
pub trait IMacroSide {
    fn echo(&self, s: &str) -> BinderResult<String>;
    fn add(&self, a: i32, b: i32) -> BinderResult<i32>;
    fn apply(&self, cfg: &MacroCfg, mode: MacroMode) -> BinderResult<MacroCfg>;
    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()>;
    fn maybe(&self, s: Option<&str>) -> BinderResult<Option<String>>;
    #[oneway]
    fn ping(&self) -> BinderResult<()>;
}

/// Service written against the macro-declared trait.
struct MacroSvc {
    pinged: Arc<Mutex<u32>>,
}
impl Interface for MacroSvc {}
impl IMacroSide for MacroSvc {
    fn echo(&self, s: &str) -> BinderResult<String> {
        Ok(format!("macro:{s}"))
    }
    fn add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn apply(&self, cfg: &MacroCfg, mode: MacroMode) -> BinderResult<MacroCfg> {
        Ok(MacroCfg {
            name: format!("{}/{mode:?}", cfg.name),
            retries: cfg.retries + 1,
            extra: cfg.extra.clone(),
        })
    }
    fn fill(&self, values: &mut Vec<i32>) -> BinderResult<()> {
        for (i, slot) in values.iter_mut().enumerate() {
            *slot = i as i32 + 100;
        }
        Ok(())
    }
    fn maybe(&self, s: Option<&str>) -> BinderResult<Option<String>> {
        Ok(s.map(str::to_uppercase))
    }
    fn ping(&self) -> BinderResult<()> {
        *self.pinged.lock().unwrap() += 1;
        Ok(())
    }
}

/// The same service written against the `.aidl`-generated trait.
struct AidlSvc {
    pinged: Arc<Mutex<u32>>,
}
impl Interface for AidlSvc {}
impl AidlIface for AidlSvc {
    fn r#echo(&self, s: &str) -> BinderResult<String> {
        Ok(format!("aidl:{s}"))
    }
    fn r#add(&self, a: i32, b: i32) -> BinderResult<i32> {
        Ok(a + b)
    }
    fn r#apply(&self, cfg: &AidlCfg, mode: AidlMode) -> BinderResult<AidlCfg> {
        Ok(AidlCfg {
            r#name: format!("{}/{}", cfg.r#name, mode.0),
            r#retries: cfg.r#retries + 1,
            r#extra: cfg.r#extra.clone(),
        })
    }
    fn r#fill(&self, values: &mut Vec<i32>) -> BinderResult<()> {
        for (i, slot) in values.iter_mut().enumerate() {
            *slot = i as i32 + 100;
        }
        Ok(())
    }
    fn r#maybe(&self, s: Option<&str>) -> BinderResult<Option<String>> {
        Ok(s.map(str::to_uppercase))
    }
    fn r#ping(&self) -> BinderResult<()> {
        *self.pinged.lock().unwrap() += 1;
        Ok(())
    }
}

struct SockPath(PathBuf);
impl SockPath {
    fn new(tag: &str) -> Self {
        let mut p = std::env::temp_dir();
        p.push(format!("rsb_cross_{}_{}.sock", tag, std::process::id()));
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

/// Macro-declared service, `.aidl`-generated proxy.
#[test]
fn aidl_client_calls_a_macro_service() {
    let sock = SockPath::new("m2a");
    let pinged = Arc::new(Mutex::new(0));
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add(
            "svc",
            BnMacroSide::new_binder(MacroSvc {
                pinged: pinged.clone(),
            }),
        )
        .expect("add")
        .spawn()
        .expect("spawn");

    // Over RPC the descriptors are checked by the interface token on the
    // first transact, not by this cast.
    let svc: Strong<dyn AidlIface> = rsbinder::connect(&sock.uri("#svc")).expect("connect");

    assert_eq!(svc.r#echo("x").unwrap(), "macro:x");
    assert_eq!(svc.r#add(40, 2).unwrap(), 42);
    assert_eq!(svc.r#maybe(Some("q")).unwrap().as_deref(), Some("Q"));
    assert_eq!(svc.r#maybe(None).unwrap(), None);

    let mut out = vec![0i32; 3];
    svc.r#fill(&mut out).unwrap();
    assert_eq!(out, vec![100, 101, 102]);

    // A parcelable built by `rsbinder-aidl` decoded by the derive, and back.
    let back = svc
        .r#apply(
            &AidlCfg {
                r#name: "c".into(),
                r#retries: 1,
                r#extra: Some(vec![9, 9]),
            },
            AidlMode::SAFE,
        )
        .unwrap();
    assert_eq!(back.r#name, "c/SAFE");
    assert_eq!(back.r#retries, 2);
    assert_eq!(back.r#extra, Some(vec![9, 9]));

    // `ping` is the last method, so a transaction-code drift between the two
    // declarations shows up here first — but only if the arrival is observed.
    svc.r#ping().unwrap();
    svc.r#echo("barrier").unwrap();
    assert_eq!(*pinged.lock().unwrap(), 1);
}

/// `.aidl`-generated service, macro-declared proxy.
#[test]
fn macro_client_calls_an_aidl_service() {
    let sock = SockPath::new("a2m");
    let pinged = Arc::new(Mutex::new(0));
    let _guard = rsbinder::serve(&sock.uri(""))
        .expect("serve")
        .add(
            "svc",
            AidlBn::new_binder(AidlSvc {
                pinged: pinged.clone(),
            }),
        )
        .expect("add")
        .spawn()
        .expect("spawn");

    let svc: Strong<dyn IMacroSide> = rsbinder::connect(&sock.uri("#svc")).expect("connect");

    assert_eq!(svc.echo("x").unwrap(), "aidl:x");
    assert_eq!(svc.add(40, 2).unwrap(), 42);
    assert_eq!(svc.maybe(Some("q")).unwrap().as_deref(), Some("Q"));
    assert_eq!(svc.maybe(None).unwrap(), None);

    let mut out = vec![0i32; 3];
    svc.fill(&mut out).unwrap();
    assert_eq!(out, vec![100, 101, 102]);

    // The `.aidl` side renders the enum as a newtype and stringifies its raw
    // value; the derive's variant is the same wire value, which is the point.
    let back = svc
        .apply(
            &MacroCfg {
                name: "c".into(),
                retries: 1,
                extra: Some(vec![9, 9]),
            },
            MacroMode::SAFE,
        )
        .unwrap();
    assert_eq!(back.name, "c/1");
    assert_eq!(back.retries, 2);
    assert_eq!(back.extra, Some(vec![9, 9]));

    svc.ping().unwrap();
    svc.echo("barrier").unwrap();
    assert_eq!(*pinged.lock().unwrap(), 1);
}

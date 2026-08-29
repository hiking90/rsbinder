// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Server side of the real-libbinder cross-check for the argument shapes the
//! AOSP fixture corpus never uses. The client
//! (`cpp/codegen_shapes_interop.cpp`) drives these methods through
//! libbinder_ndk's own `AParcel_*` helpers, so the wire layout under test
//! is the one AOSP's runtime defines, not one this repo asserts.
//!
//! The rsbinder-to-rsbinder round trip in `tests/tests/codegen_shapes.rs`
//! cannot see a layout both sides agree on wrongly; this can.

use env_logger::Env;
use rsbinder::*;

use example_hello::shapes::{BnCodegenShapes, ICodegenShapes, SERVICE_NAME};

struct Shapes;

impl Interface for Shapes {}

impl ICodegenShapes for Shapes {
    fn r#takeOutBinder(
        &self,
        src: &SIBinder,
        fill: bool,
        dst: &mut Option<SIBinder>,
    ) -> rsbinder::status::Result<()> {
        // `fill == false` leaves the non-nullable `out` unset, which the
        // generated server arm must turn into UNEXPECTED_NULL.
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

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("info")).init();

    let server = rsbinder::serve("binder://")?;
    let service = BnCodegenShapes::new_binder(Shapes);
    let server = server.add(SERVICE_NAME, &service)?;
    eprintln!("SHAPES_SERVICE_READY {SERVICE_NAME}");
    Ok(server.run()?)
}

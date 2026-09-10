// Copyright 2026 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Storing an AIDL parcelable in a file instead of sending it in a
//! transaction, with [`rsbinder::to_bytes`] / [`rsbinder::from_bytes`].
//!
//! The types come from `aidl/settings/*.aidl` and are compiled by `build.rs`
//! like any other interface — nothing here is specific to storage. That is the
//! point: an AIDL definition is already a schema, and the generated code is
//! already a complete serializer, so a project need not describe the same data
//! a second time in protobuf or serde.
//!
//! ```text
//! cargo run -p example-hello --features rpc --bin serde_demo
//! ```
//!
//! No binder device, no service manager, no socket — this talks to nothing.
//! The `rpc` feature is required only because `to_bytes` encodes in the
//! session-less parcel mode that feature carries.
//!
//! See `book/src/data-serialization.md`.

use example_hello::settings::{
    Endpoint::Endpoint, Mode::Mode, SettingsV1::SettingsV1, SettingsV2::SettingsV2, STORE_PATH,
};
use rsbinder::{from_bytes, to_bytes, StatusCode};

fn sample() -> SettingsV2 {
    SettingsV2 {
        name: "studio".into(),
        volume: 72,
        mode: Mode::ON,
        endpoint: Endpoint {
            host: "10.0.0.4".into(),
            port: 9000,
        },
        tags: vec!["fast".into(), "한글".into()],
        note: Some("written by v2".into()),
        retries: 5,
        updatedAtMillis: 1_757_000_000_000,
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    // 1. Write a value to a file, read it back, compare.
    let original = sample();
    let bytes = to_bytes(&original)?;
    std::fs::write(STORE_PATH, &bytes)?;
    println!("wrote {} bytes to {STORE_PATH}", bytes.len());

    let restored: SettingsV2 = from_bytes(&std::fs::read(STORE_PATH)?)?;
    assert_eq!(restored, original);
    println!("read back: {restored:?}");

    // 2. A reader built against the OLDER definition reads the newer bytes.
    //    The parcelable's length header tells it where the value ends, so the
    //    two fields v2 appended are skipped rather than misread.
    let as_v1: SettingsV1 = from_bytes(&bytes)?;
    assert_eq!(as_v1.name, original.name);
    assert_eq!(as_v1.volume, original.volume);
    assert_eq!(as_v1.mode, original.mode);
    assert_eq!(as_v1.endpoint, original.endpoint);
    assert_eq!(as_v1.tags, original.tags);
    assert_eq!(as_v1.note, original.note);
    println!("v1 reader saw v2 bytes, extra fields skipped: {as_v1:?}");

    // 3. A reader built against the NEWER definition reads the older bytes.
    //    The fields v1 never wrote come back as the AIDL defaults —
    //    `retries = 3` from the `.aidl`, and `0` for the field with none.
    let v1_value = SettingsV1 {
        name: "laptop".into(),
        volume: 30,
        mode: Mode::OFF,
        endpoint: Endpoint::default(),
        tags: vec!["quiet".into()],
        note: None,
    };
    let v1_bytes = to_bytes(&v1_value)?;
    let as_v2: SettingsV2 = from_bytes(&v1_bytes)?;
    assert_eq!(as_v2.name, v1_value.name);
    assert_eq!(as_v2.retries, 3, "the .aidl default");
    assert_eq!(as_v2.updatedAtMillis, 0);
    println!("v2 reader saw v1 bytes, missing fields defaulted: {as_v2:?}");

    // 4. Reading is strict: the input must be consumed exactly. Leftover
    //    bytes usually mean the file was written as a different type, and
    //    returning the partial value would hide that behind a plausible
    //    wrong answer.
    let mut trailing = bytes.clone();
    trailing.push(0);
    assert_eq!(
        from_bytes::<SettingsV2>(&trailing).unwrap_err(),
        StatusCode::BadValue
    );
    assert_eq!(
        from_bytes::<SettingsV2>(&bytes[..bytes.len() - 4]).unwrap_err(),
        StatusCode::NotEnoughData
    );
    println!("trailing bytes -> BadValue, truncated -> NotEnoughData");

    // 5. What cannot be stored. A file descriptor means nothing outside the
    //    process that opened it, so it is refused where it is written —
    //    before any `dup` — rather than encoded into something that refers
    //    to nothing. A binder is refused the same way, as `BadType`.
    let devnull = rsbinder::ParcelFileDescriptor::new(std::fs::File::open("/dev/null")?);
    assert_eq!(to_bytes(&devnull).unwrap_err(), StatusCode::FdsNotAllowed);
    println!("a file descriptor is refused at write time: FdsNotAllowed");

    // 6. The bytes are the IPC bytes, and the parcel wire is little-endian on
    //    every host — so this file reads back on any machine, not only one
    //    with the same byte order.
    println!(
        "volume ({}) is stored as {:02x?}",
        original.volume,
        original.volume.to_le_bytes()
    );

    std::fs::remove_file(STORE_PATH)?;
    Ok(())
}

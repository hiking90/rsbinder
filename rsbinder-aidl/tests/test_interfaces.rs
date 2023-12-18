// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

use rsbinder_aidl::Builder;
use std::path::PathBuf;
use std::error::Error;

#[test]
fn test_service_manager() -> Result<(), Box<dyn Error>> {
    Builder::new()
        .source(PathBuf::from("../aidl/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("../aidl/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("../aidl/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("../aidl/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("../aidl/android/os/ServiceDebugInfo.aidl"))
        // .source(PathBuf::from("../aidl/android/os/PersistableBundle.aidl"))
        // .source(PathBuf::from("../aidl/android/content/AttributionSource.aidl"))
        // .source(PathBuf::from("../aidl/android/aidl/tests"))
        .generate()?;

    Ok(())
}

#[test]
fn test_aidl_tests() -> Result<(), Box<dyn Error>> {
    Builder::new()
        .source(PathBuf::from("../aidl/android/os/PersistableBundle.aidl"))
        .source(PathBuf::from("../aidl/android/content/AttributionSource.aidl"))
        .source(PathBuf::from("../aidl/android/aidl/tests"))
        .output(PathBuf::from("AidlTests.rs"))
        .generate()?;

    Ok(())
}

#[test]
fn test_list_of_interfaces() -> Result<(), Box<dyn Error>> {
    Builder::new()
        .source(PathBuf::from("../aidl/android/aidl/tests/ListOfInterfaces.aidl"))
        .output(PathBuf::from("ListOfInterfaces.rs"))
        .generate()?;

    Ok(())
}

#[test]
fn test_array_of_interfaces() -> Result<(), Box<dyn Error>> {
    Builder::new()
        .source(PathBuf::from("../aidl/android/aidl/tests/ArrayOfInterfaces.aidl"))
        .output(PathBuf::from("ArrayOfInterfaces.rs"))
        .generate()?;

    Ok(())
}

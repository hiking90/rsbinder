// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

//! Legacy comprehensive test file for rsbinder-aidl AIDL code generation tests
//!
//! This file previously contained large comprehensive tests that have been split into focused modules:
//! - test_arrays.rs: Array and nullability tests
//! - test_interfaces.rs: Interface array tests
//! - test_enums_unions.rs: Enum and union tests
//! - test_parcelables.rs: Parcelable tests
//! - test_enum_references.rs: Enum reference tests (already existed)
//!
//! This file is kept for backward compatibility and can be removed once we're confident
//! the split tests work correctly across all scenarios.

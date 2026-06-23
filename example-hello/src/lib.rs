// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Include the AIDL-generated code and flatten `IHello`'s items (the trait,
// `BnHello`, `BpHello`, …) into this module — one call for the include +
// `pub use` pair.
rsbinder::include_aidl!("hello", crate::hello::IHello::*);

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";

/// `ICallingIdentity` calling-identity interop fixture.
pub mod calling_identity {
    rsbinder::include_aidl!("calling_identity", self::calling_identity::ICallingIdentity::*);
    pub const SERVICE_NAME: &str = "rsbinder.test.calling_identity";
}

/// `@EnforcePermission` codegen E2E fixture
/// (`IPermCheck`). Built solely so the generated `on_transact` arms
/// type-check against `rsbinder::permission_controller::check_permission`
/// and against the `Status` / `ExceptionCode::Security` deny path. The
/// service implementation lives in
/// [`example-hello/src/bin/enforce_permission_interop_service.rs`](../bin/enforce_permission_interop_service.rs).
pub mod permcheck {
    rsbinder::include_aidl!("permcheck", self::permcheck::IPermCheck::*);
    pub const SERVICE_NAME: &str = "rsbinder.test.permcheck";
}

/// `TF_UPDATE_TXN` async-dedup fixture.
pub mod update_txn {
    rsbinder::include_aidl!("update_txn", self::update_txn::IUpdateTxnDedup::*);
    pub const SERVICE_NAME: &str = "rsbinder.test.update_txn";
    /// The dedup interop client builds its own oneway parcel so it can
    /// OR in `FLAG_UPDATE_TXN` (the AIDL-generated `Bp*` stub does not
    /// expose a flag override). Re-expose the generated transaction
    /// code so we keep using the canonical opcode constant.
    pub const ONRECORD_CODE: rsbinder::TransactionCode =
        self::update_txn::IUpdateTxnDedup::transactions::r#onRecord;
}

/// RT inheritance fixture.
pub mod rt_inherit {
    rsbinder::include_aidl!("rt_inherit", self::rt_inherit::IRtCheck::*);
    pub const SERVICE_NAME: &str = "rsbinder.test.rt_inherit";
}

/// Plan 2-16 handler-side authorization example (`bin/authz_{service,client}`).
pub mod authz {
    rsbinder::include_aidl!("authz", self::authz::IAuthz::*);
    pub const SERVICE_NAME: &str = "example.authz";
    /// Unix-domain socket the example service binds and client connects to.
    pub const RPC_SOCKET: &str = "/tmp/rsb_authz.sock";
}

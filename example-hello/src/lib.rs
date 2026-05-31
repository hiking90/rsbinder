// Copyright 2022 Jeff Kim <hiking90@gmail.com>
// SPDX-License-Identifier: Apache-2.0

// Include the code hello.rs generated from AIDL.
include!(concat!(env!("OUT_DIR"), "/hello.rs"));

// Set up to use the APIs provided in the code generated for Client and Service.
pub use crate::hello::IHello::*;

// Define the name of the service to be registered in the HUB(service manager).
pub const SERVICE_NAME: &str = "my.hello";

/// `ICallingIdentity` calling-identity interop fixture.
pub mod calling_identity {
    include!(concat!(env!("OUT_DIR"), "/calling_identity.rs"));
    pub use self::calling_identity::ICallingIdentity::*;
    pub const SERVICE_NAME: &str = "rsbinder.test.calling_identity";
}

/// `@EnforcePermission` codegen E2E fixture
/// (`IPermCheck`). Built solely so the generated `on_transact` arms
/// type-check against `rsbinder::permission_controller::check_permission`
/// and against the `Status` / `ExceptionCode::Security` deny path. The
/// service implementation lives in
/// [`example-hello/src/bin/enforce_permission_interop_service.rs`](../bin/enforce_permission_interop_service.rs).
pub mod permcheck {
    include!(concat!(env!("OUT_DIR"), "/permcheck.rs"));
    pub use self::permcheck::IPermCheck::*;
    pub const SERVICE_NAME: &str = "rsbinder.test.permcheck";
}

/// `TF_UPDATE_TXN` async-dedup fixture.
pub mod update_txn {
    include!(concat!(env!("OUT_DIR"), "/update_txn.rs"));
    pub use self::update_txn::IUpdateTxnDedup::*;
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
    include!(concat!(env!("OUT_DIR"), "/rt_inherit.rs"));
    pub use self::rt_inherit::IRtCheck::*;
    pub const SERVICE_NAME: &str = "rsbinder.test.rt_inherit";
}

/// Plan 2-16 handler-side authorization example (`bin/authz_{service,client}`).
pub mod authz {
    include!(concat!(env!("OUT_DIR"), "/authz.rs"));
    pub use self::authz::IAuthz::*;
    pub const SERVICE_NAME: &str = "example.authz";
    /// Unix-domain socket the example service binds and client connects to.
    pub const RPC_SOCKET: &str = "/tmp/rsb_authz.sock";
}

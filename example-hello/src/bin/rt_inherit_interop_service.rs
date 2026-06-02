// SPDX-License-Identifier: Apache-2.0
//
// RT priority inheritance verification.
//
// The server advertises its binder with
// `BinderFeatures { inherit_rt: true, min_sched_policy: Some(SCHED_FIFO),
// min_priority: Some(...) }`. When a client running under SCHED_FIFO
// calls `reportSchedPolicy()`, the kernel's binder driver lifts the
// worker thread's scheduler class for the duration of the handler.
// The handler asks the kernel back via `sched_getscheduler(0)` and
// returns the result; the client checks it matches expectations.

use rsbinder::service::{kernel, Registry as _};
use rsbinder::*;

use example_hello::rt_inherit::{BnRtCheck, IRtCheck, SERVICE_NAME};

struct RtCheck;

impl Interface for RtCheck {}

impl IRtCheck for RtCheck {
    fn reportSchedPolicy(&self) -> rsbinder::status::Result<i32> {
        let policy = rsbinder::get_current_scheduler_policy()
            .map_err(|_| Status::from(ExceptionCode::IllegalState))?;
        Ok(policy)
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    eprintln!("STAGE3 4-5 server: init ProcessState");
    let host = kernel::Host::new()?;

    // Advertise SCHED_FIFO floor + inherit_rt so the driver lifts the
    // binder worker into RT for any incoming transaction. The
    // `BinderFeatures` live on `new_binder_with_features`, independent of
    // the facade — only the init/register/serve scaffolding moved here.
    let mut features = BinderFeatures::default();
    features.min_sched_policy = Some(libc::SCHED_FIFO);
    features.min_priority = Some(5);
    features.inherit_rt = true;
    let service = BnRtCheck::new_binder_with_features(RtCheck, features);

    eprintln!("STAGE3 4-5 server: register `{SERVICE_NAME}` (SCHED_FIFO/5, inherit_rt)");
    host.add_service(SERVICE_NAME, service.as_binder())?;

    eprintln!("STAGE3 4-5 server: join thread pool");
    Ok(host.serve()?)
}

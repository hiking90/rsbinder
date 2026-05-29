// SPDX-License-Identifier: Apache-2.0
//
// `TF_UPDATE_TXN` dedup recorder.
//
// Registers `rsbinder.test.update_txn`. The async `onRecord` handler
// sleeps for the requested duration before appending the payload to an
// internal `Mutex<Vec<i32>>`. With only one pooled worker, an inbound
// async call has to sit in the driver's `async_todo` queue until the
// current call's handler returns — that's exactly the window in which
// `FLAG_UPDATE_TXN` (set by the client) makes a newer same-`(target,
// code)` call *replace* the queued one instead of stacking. The
// client's later `drain()` exposes the surviving payloads.

use std::sync::Mutex;

use rsbinder::*;

use example_hello::update_txn::{BnUpdateTxnDedup, IUpdateTxnDedup, SERVICE_NAME};

struct Recorder {
    recorded: Mutex<Vec<i32>>,
}

impl Interface for Recorder {}

impl IUpdateTxnDedup for Recorder {
    fn onRecord(&self, v: i32, delay_ms: i32) -> rsbinder::status::Result<()> {
        if delay_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay_ms as u64));
        }
        self.recorded.lock().unwrap().push(v);
        Ok(())
    }

    fn drain(&self) -> rsbinder::status::Result<Vec<i32>> {
        Ok(self.recorded.lock().unwrap().clone())
    }

    fn reset(&self) -> rsbinder::status::Result<()> {
        self.recorded.lock().unwrap().clear();
        Ok(())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    // Force a single worker — the kernel cannot dispatch async
    // transactions in parallel, which is exactly the window in which
    // `TF_UPDATE_TXN` collapses queued duplicates. `max_threads=1`
    // tells `BINDER_SET_MAX_THREADS` the driver may request at most
    // *one* looper beyond the main thread, and we skip
    // `start_thread_pool()` so we never pre-spawn one. The main thread
    // then becomes the sole consumer once it enters
    // `join_thread_pool()`.
    eprintln!("STAGE3 4-4 server: init ProcessState (max_threads=1, single worker)");
    ProcessState::init(rsbinder::DEFAULT_BINDER_PATH, 1)?;

    let service = BnUpdateTxnDedup::new_binder(Recorder {
        recorded: Mutex::new(Vec::new()),
    });

    eprintln!("STAGE3 4-4 server: register `{SERVICE_NAME}`");
    hub::add_service(SERVICE_NAME, service.as_binder())?;

    eprintln!("STAGE3 4-4 server: join thread pool");
    Ok(ProcessState::join_thread_pool()?)
}

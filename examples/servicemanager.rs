use std::sync::Arc;
use std::io;
use std::os::unix::io::{AsRawFd, RawFd, IntoRawFd};

use tokio::io::{unix::AsyncFd, Interest};
use rsbinder::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    {
        let mut process_state = ProcessState::as_self().write().unwrap();

        process_state.init(DEFAULT_BINDER_PATH, 0);
        process_state.become_context_manager();
    }

    let async_fd = AsyncFd::with_interest(ProcessState::as_self().read()?.as_raw_fd(), Interest::READABLE)?;

    thread_state::setup_polling().expect("Failed in ThreadState::setup_polling()");

    loop {
        async_fd.readable().await?.clear_ready();
        thread_state::handle_commands().expect("Failed in ThreadState::handle_commands()");
    }
}
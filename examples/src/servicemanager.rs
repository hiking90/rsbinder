use tokio::io::{unix::AsyncFd, Interest};
use rsbinder::*;

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    ProcessState::init(DEFAULT_BINDER_PATH, 0).become_context_manager();

    let async_fd = AsyncFd::with_interest(ProcessState::as_self().driver(), Interest::READABLE)?;

    thread_state::setup_polling().expect("Failed in ThreadState::setup_polling()");

    loop {
        async_fd.readable().await?.clear_ready();
        thread_state::handle_commands().expect("Failed in ThreadState::handle_commands()");
    }
}
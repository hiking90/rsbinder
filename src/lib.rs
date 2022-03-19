mod sys;
mod process_state;
mod thread_state;
mod parcel;
mod error;
pub mod binderfs;

pub use process_state::ProcessState;
pub use thread_state::{ThreadState, THREAD_STATE};
pub use parcel::Parcel;

pub use error::{ErrorKind, Error, Result};

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";

#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn process_state() {
        let mut process = ProcessState::as_self().write().unwrap();
        process.init("/dev/binderfs/binder", 0);
    }

    #[test]
    fn thread_state() {
        process_state();
        THREAD_STATE.with(|_state| {

        });
    }
}

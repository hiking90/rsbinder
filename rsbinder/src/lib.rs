#[macro_use]
extern crate lazy_static;

mod sys;
mod process_state;
pub mod thread_state;
mod error;
mod macros;
pub mod native;
mod binder;
pub mod parcel;
pub mod binderfs;
mod service_manager;
pub mod parcelable;
pub mod proxy;

pub use process_state::ProcessState;
// pub use thread_state::Se;
pub use parcel::Parcel;
pub use error::{StatusCode, Error, Result, ExceptionCode};
pub use binder::*;
pub use service_manager::*;
pub use proxy::*;
// pub use ref_base::*;

pub const DEFAULT_BINDER_CONTROL_PATH: &str = "/dev/binderfs/binder-control";
pub const DEFAULT_BINDER_PATH: &str = "/dev/binderfs/binder";


#[cfg(test)]
mod tests {
    use crate::*;

    #[test]
    fn process_state() {
        let process = ProcessState::as_self();
        process.init("/dev/binderfs/binder", 0);
    }

    #[test]
    fn thread_state() {
        process_state();
    }
}

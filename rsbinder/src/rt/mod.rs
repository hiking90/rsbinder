#[cfg(feature = "tokio")]
mod tokio_rt;
#[cfg(feature = "tokio")]
mod wait_threads;
#[cfg(feature = "tokio")]
pub use tokio_rt::*;
#[cfg(all(test, feature = "tokio", any(target_os = "linux", target_os = "android")))]
pub(crate) use wait_threads::REFUSE_WAIT_THREAD;
#[cfg(feature = "tokio")]
pub use wait_threads::WaitThreads;

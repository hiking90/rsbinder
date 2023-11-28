include!(concat!(env!("OUT_DIR"), "/hello.rs"));

pub use crate::hello::IHello::*;

pub const SERVICE_NAME: &str = "my.hello";
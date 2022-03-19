use std::path::Path;
use rsbinder::*;
use env_logger;

fn main() -> std::io::Result<()>{
    env_logger::init();

    let device_name = "binder2";
    binderfs::add_device(Path::new(DEFAULT_BINDER_CONTROL_PATH), device_name)
        .map(|(major, minor)| {
            println!("Allocated new binder device with major {}, minor {}, and name {}", major, minor, device_name);
        })?;

    Ok(())
}
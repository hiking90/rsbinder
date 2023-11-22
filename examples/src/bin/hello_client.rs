// include!("../rsbinder_generated.rs");

use rsbinder_hub::IServiceManager;
use env_logger::Env;
use rsbinder::*;
// use rsbinder_hub::*;

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let process = ProcessState::as_self();
    process.init(DEFAULT_BINDER_PATH, 0);
    let hub = rsbinder_hub::default();

    println!("list services:");
    for name in hub.listServices(15)? {
        println!("{}", name);
    }

    Ok(())
}
use env_logger::Env;
use rsbinder::*;

fn main() -> Result<()> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    let process = ProcessState::as_self();
    process.init(DEFAULT_BINDER_PATH, 0);

    let service_manager = process.context_object();
    if let Err(err) = service_manager {
        println!("{:?}", err);
    }

    Ok(())
}
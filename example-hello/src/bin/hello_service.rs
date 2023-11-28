use rsbinder_hub::IServiceManager;
use env_logger::Env;
use rsbinder::*;

use example_hello::*;

struct IHelloService;

impl Interface for IHelloService {}

impl IHello for IHelloService {
    fn echo(&self, echo: &str) -> rsbinder::Result<String> {
        Ok(echo.to_owned())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init(DEFAULT_BINDER_PATH, 0);

    let service = BnHello::new_binder(IHelloService{});

    let hub = rsbinder_hub::default();
    hub.addService(SERVICE_NAME, &service.as_binder(), false, rsbinder_hub::DUMP_FLAG_PRIORITY_DEFAULT)?;

    Ok(ProcessState::join_thread_pool()?)
}

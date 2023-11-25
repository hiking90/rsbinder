use std::fs::File;
use rsbinder_hub::IServiceManager;
use env_logger::Env;
use rsbinder::*;
// use rsbinder_::*;

pub trait IEcho: Interface {
    fn echo(&self, echo: &str) -> rsbinder::Result<String>;
}

pub struct BnIEcho(Box<dyn IEcho + Sync + Send + 'static>);

impl IEcho for native::Binder<BnIEcho> {
    fn echo(&self, echo: &str) -> rsbinder::Result<String> {
        self.0.echo(echo)
    }
}

impl Remotable for BnIEcho {
    fn get_descriptor() -> &'static str where Self: Sized {
        "my.hello.Echo"
    }
    fn on_transact(&self, code: TransactionCode, reader: &mut Parcel, reply: &mut Parcel) -> Result<()> {
        todo!("on_transact")
    }
    fn on_dump(&self, file: &File, args: &[&str]) -> Result<()> {
        todo!("BnIEcho on_dump")
    }
}

// fn on_transact(
//     service: &dyn ITest,
//     code: TransactionCode,
//     _data: &BorrowedParcel,
//     reply: &mut BorrowedParcel,
// ) -> binder::Result<()> {
//     match code {
//         SpIBinder::FIRST_CALL_TRANSACTION => {
//             reply.write(&service.test()?)?;
//             Ok(())
//         }
//         _ => Err(StatusCode::UNKNOWN_TRANSACTION),
//     }
// }


struct IEchoService;

impl Interface for IEchoService {
    // fn box_clone(&self) -> std::boxed::Box<(dyn rsbinder::Interface + 'static)> { todo!() }
}

impl IEcho for IEchoService {
    fn echo(&self, echo: &str) -> rsbinder::Result<String> {
        Ok(echo.to_owned())
    }
}

fn main() -> std::result::Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(Env::default().default_filter_or("warn")).init();

    ProcessState::init(DEFAULT_BINDER_PATH, 0)?;
    let hub = rsbinder_hub::default();

    println!("list services:");
    for name in hub.listServices(rsbinder_hub::DUMP_FLAG_PRIORITY_ALL)? {
        println!("{}", name);
    }

    Ok(())
}
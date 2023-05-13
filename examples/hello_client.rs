use rsbinder::*;
// use std::backtrace::Backtrace;
// use ctrlc;


fn main() -> Result<()> {
    // ctrlc::set_handler(move || {
    //     let bt = Backtrace::force_capture();
    //     println!("{:?}", bt);
    //     std::process::exit(0);
    // }).unwrap();

    let process = ProcessState::as_self();

    let service_manager = process.context_object()?;

    Ok(())
}
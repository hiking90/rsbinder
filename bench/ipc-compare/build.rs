use std::path::PathBuf;

fn main() {
    let sources = [
        "aidl/ipccompare/BlobBytes.aidl",
        "aidl/ipccompare/StreamBytes.aidl",
        "aidl/ipccompare/IBenchSink.aidl",
        "aidl/ipccompare/IBench.aidl",
    ];
    for source in sources {
        println!("cargo:rerun-if-changed={source}");
    }
    let builder = sources.iter().fold(
        rsbinder_aidl::Builder::new().set_async_support(true),
        |builder, source| builder.source(PathBuf::from(source)),
    );
    if let Err(err) = builder.output(PathBuf::from("bench.rs")).generate() {
        eprintln!("generating bench.rs: {err:?}");
        std::process::exit(1);
    }
}

use std::path::PathBuf;

fn main() {
    rsbinder_aidl::Builder::new()
        .source(PathBuf::from("../rsbinder-aidl/aidl/android/os/IServiceManager.aidl"))
        .source(PathBuf::from("../rsbinder-aidl/aidl/android/os/IClientCallback.aidl"))
        .source(PathBuf::from("../rsbinder-aidl/aidl/android/os/IServiceCallback.aidl"))
        .source(PathBuf::from("../rsbinder-aidl/aidl/android/os/ConnectionInfo.aidl"))
        .source(PathBuf::from("../rsbinder-aidl/aidl/android/os/ServiceDebugInfo.aidl"))
        .generate().unwrap();
}
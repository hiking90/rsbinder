use rsbinder_aidl::Builder;
use std::path::PathBuf;
use std::error::Error;
use similar::{ChangeTag, TextDiff};


#[test]
fn test_list_of_interfaces() -> Result<(), Box<dyn Error>> {
    // aidl_generator(r##"
    //     "##,
    //     r#"
    //     "#)?;

    Builder::new()
        .source(PathBuf::from("../aidl/android/aidl/tests/ListOfInterfaces.aidl"))
        .output(PathBuf::from("ListOfInterfaces.rs"))
        .generate()?;

    Ok(())
}

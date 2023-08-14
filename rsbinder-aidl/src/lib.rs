#[macro_use]
extern crate lazy_static;

use std::error::Error;
use std::path::{Path, PathBuf};
use std::fs;
use convert_case::{Case, Casing};

mod parser;
mod generator;
mod const_expr;

pub const DEFAULT_NAMESPACE: &str = "aidl";

pub fn indent_space(step: usize) -> String {
    let indent = "    ";
    let mut ret = String::new();

    for _ in 0..step {
        ret += indent;
    }

    ret
}

fn add_indent(step: usize, source: &str) -> String {
    let mut content = String::new();
    for line in source.lines() {
        if line.len() > 0 {
            content += &(indent_space(step) + line + "\n");
        } else {
            content += "\n";
        }
    }
    content
}

fn add_namespace(namespace: &str, source: &str) -> String {
    let mut content = String::new();

    content += &format!("pub mod {} {{\n", namespace);

    content += &add_indent(1, source);

    content += "}\n";

    content
}

pub struct Builder {
    sources: Vec<PathBuf>,
    dest_dir: PathBuf,
}

impl Builder {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            dest_dir: PathBuf::from(std::env::var_os("OUT_DIR").unwrap_or("aidl_gen".into())),
        }
    }

    pub fn new_with_destination(dest: PathBuf) -> Self {
        Self {
            sources: Vec::new(),
            dest_dir: dest,
        }
    }

    pub fn source(mut self, source: PathBuf) -> Self {
        self.sources.push(source);
        self
    }

    fn parse_file(filename: &Path) -> Result<(String, parser::Document), Box<dyn Error>> {
        let unparsed_file = fs::read_to_string(filename.clone())?;
        let document = parser::parse_document(&unparsed_file)?;

        Ok((filename.file_stem().unwrap().to_str().unwrap().to_string(), document))

        // let package = generator::gen_document(&document)?;

        // Ok((package.0, package.1, filename.file_stem().unwrap().to_str().unwrap().to_string()))
    }

    fn traverse_source(&self, dir: &Path) -> Result<Vec<(String, parser::Document)>, Box<dyn Error>> {
        let entries = fs::read_dir(dir).unwrap();
        let mut package_list = Vec::new();

        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                package_list.append(&mut self.traverse_source(&path)?);
            }
            if path.is_file() && path.extension().unwrap_or_default() == "aidl" {
                let package = Self::parse_file(&path)?;
                package_list.push(package);
            }
        }

        Ok(package_list)
    }

    fn generate_all(&self, mut package_list: Vec<(String, String, String)>) -> Result<String, Box<dyn Error>> {
        let mut content = String::new();
        let mut namespace = String::new();
        let mut mod_count:usize = 0;

        content += "#[allow(unused_imports)]\n\n";

        package_list.sort();

        for package in package_list {
            if namespace != package.0 {
                let namespace_split: Vec<&str> = namespace.split('.').collect();
                let mod_list: Vec<&str> = package.0.split('.').collect();

                let cmp_len = std::cmp::min(namespace_split.len(), mod_list.len());
                let mut start = 0;

                for i in 0..cmp_len {
                    if namespace_split[i] == mod_list[i] {
                        start += 1;
                    } else {
                        break;
                    }
                }

                for i in (start..mod_count).rev() {
                    content += &indent_space(i);
                    content += "}\n";
                }

                namespace = package.0.clone();
                mod_count = start;

                for r#mod in &mod_list[start..] {
                    content += &indent_space(mod_count);
                    content += &format!("pub mod {} {{\n", r#mod);
                    mod_count += 1;
                }

            }

            content += "\n";
            content += &(indent_space(mod_count) + &format!("pub use {}::*;\n", package.2.to_case(Case::Snake)));

            content += &add_indent(mod_count, &package.1);
        }

        for i in (0..mod_count).rev() {
            content += &indent_space(i);
            content += "}\n";
        }

        Ok(content)
    }

    pub fn generate(self) -> Result<(), Box<dyn Error>> {
        let mut document_list = Vec::new();

        for source in &self.sources {
            if source.is_file() {
                let package = Self::parse_file(&source)?;
                document_list.push(package);
            } else {
                document_list.append(&mut self.traverse_source(&source)?);
            };
        }

        let mut package_list = Vec::new();
        for document in document_list {
            let package = generator::gen_document(&document.1)?;
            package_list.push((package.0, package.1, document.0));
        }

        let content = self.generate_all(package_list)?;
        let content = add_namespace(DEFAULT_NAMESPACE, &content);

        let output = self.dest_dir.join("rsbinder_generated_aidl.rs");
        println!("==== {output:?} ====");
        fs::write(output, content)?;

        Ok(())
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_service_manager() -> Result<(), Box<dyn Error>> {
        Builder::new()
            .source(PathBuf::from("../aidl/android/os/IServiceManager.aidl"))
            .source(PathBuf::from("../aidl/android/os/IClientCallback.aidl"))
            .source(PathBuf::from("../aidl/android/os/IServiceCallback.aidl"))
            .source(PathBuf::from("../aidl/android/os/ConnectionInfo.aidl"))
            .source(PathBuf::from("../aidl/android/os/ServiceDebugInfo.aidl"))
            .generate()?;

        Ok(())
    }

    #[test]
    fn test_builder() -> Result<(), Box<dyn Error>> {
        Builder::new()
            .source(PathBuf::from("aidl"))
            .generate()
    }

    fn enum_generator(input: &str, expect: &str) -> Result<(), Box<dyn Error>> {
        let document = parser::parse_document(input)?;
        let res = generator::gen_document(&document)?;
        assert_eq!(res.1.trim(), expect.trim());
        Ok(())
    }

    #[test]
    fn test_enum() -> Result<(), Box<dyn Error>> {
        enum_generator(r##"
                @Backing(type="byte")
                enum ByteEnum {
                    // Comment about FOO.
                    FOO = 1,
                    BAR = 2,
                    BAZ,
                }
            "##,
            r##"
declare_binder_enum! {
    ByteEnum : [i8; 3] {
        FOO = 1,
        BAR = 2,
        BAZ = 3,
    }
}
            "##)?;

        enum_generator(r##"
                enum BackendType {
                    CPP,
                    JAVA,
                    NDK,
                    RUST,
                }
            "##,
            r##"
declare_binder_enum! {
    BackendType : [i8; 4] {
        CPP = 0,
        JAVA = 1,
        NDK = 2,
        RUST = 3,
    }
}
            "##)?;

        enum_generator(r##"
            @Backing(type="int")
            enum ConstantExpressionEnum {
                // Should be all true / ones.
                // dec literals are either int or long
                decInt32_1 = (~(-1)) == 0,
                decInt32_2 = ~~(1 << 31) == (1 << 31),
                decInt64_1 = (~(-1L)) == 0,
                decInt64_2 = (~4294967295L) != 0,
                decInt64_3 = (~4294967295) != 0,
                decInt64_4 = ~~(1L << 63) == (1L << 63),

                // hex literals could be int or long
                // 0x7fffffff is int, hence can be negated
                hexInt32_1 = -0x7fffffff < 0,

                // 0x80000000 is int32_t max + 1
                hexInt32_2 = 0x80000000 < 0,

                // 0xFFFFFFFF is int32_t, not long; if it were long then ~(long)0xFFFFFFFF != 0
                hexInt32_3 = ~0xFFFFFFFF == 0,

                // 0x7FFFFFFFFFFFFFFF is long, hence can be negated
                hexInt64_1 = -0x7FFFFFFFFFFFFFFF < 0
            }
            "##,

        // TODO: Android AIDL generates 1, but rsbinder aidl generates 0.
        // hexInt32_2 = 0,
        // hexInt32_3 = 0,

            r##"
declare_binder_enum! {
    ConstantExpressionEnum : [i32; 10] {
        decInt32_1 = 1,
        decInt32_2 = 1,
        decInt64_1 = 1,
        decInt64_2 = 1,
        decInt64_3 = 1,
        decInt64_4 = 1,
        hexInt32_1 = 1,
        hexInt32_2 = 0,
        hexInt32_3 = 0,
        hexInt64_1 = 1,
    }
}
            "##)?;

        enum_generator(r##"
            @Backing(type="int")
            enum IntEnum {
                FOO = 1000,
                BAR = 2000,
                BAZ,
                /** @deprecated do not use this */
                QUX,
            }
            "##,
            r##"
declare_binder_enum! {
    IntEnum : [i32; 4] {
        FOO = 1000,
        BAR = 2000,
        BAZ = 2001,
        QUX = 2002,
    }
}
            "##)?;

        enum_generator(r##"
            @Backing(type="long")
            enum LongEnum {
                FOO = 100000000000,
                BAR = 200000000000,
                BAZ,
            }
            "##,
            r##"
declare_binder_enum! {
    LongEnum : [i64; 3] {
        FOO = 100000000000,
        BAR = 200000000000,
        BAZ = 200000000001,
    }
}
            "##)?;

        Ok(())
    }

//     #[test]
//     fn test_gen_include_all() -> Result<(), Box<dyn Error>> {
//         let builder = Builder::new();
//         let package_list = vec![
//             ("android.os".to_owned(), "IServiceManager".to_owned()),
//             ("android.os.callback".to_owned(), "IClientCallback".to_owned()),
//             ("android.graphics".to_owned(), "Bitmap".to_owned()),
//             ("rsbinder.hello".to_owned(), "World".to_owned()),
//         ];

//         let content = builder.gen_include_all(package_list)?;

//         assert_eq!(content.trim(),
//             r##"
// pub mod android {
//     pub mod graphics {
//         include!(concat!(std::env!("OUT_DIR"), "/aidl_gen/android/graphics/Bitmap"));
//     }
//     pub mod os {
//         include!(concat!(std::env!("OUT_DIR"), "/aidl_gen/android/os/IServiceManager"));
//         pub mod callback {
//             include!(concat!(std::env!("OUT_DIR"), "/aidl_gen/android/os/callback/IClientCallback"));
//         }
//     }
// }
// pub mod rsbinder {
//     pub mod hello {
//         include!(concat!(std::env!("OUT_DIR"), "/aidl_gen/rsbinder/hello/World"));
//     }
// }
//             "##.trim());

//         Ok(())
//     }
}
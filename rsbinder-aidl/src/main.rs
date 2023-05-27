use pest::Parser;
#[derive(pest_derive::Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

use std::fs;
use clap::Parser as ClapParser;

#[derive(ClapParser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    #[arg(short, long)]
    filename: String,
}

fn parse_str(aidl: &str) {
    match AIDLParser::parse(Rule::document, &aidl) {
        Ok(res) => {
            println!("{}", res);
        },
        Err(err) => {
            println!("{}", err);
        }
    }

}


fn main() {
    let args = Args::parse();

    let unparsed_file = fs::read_to_string(args.filename).expect("cannot read file");
    parse_str(&unparsed_file);
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use super::*;

    fn parse_str(aidl: &str) -> Result<(), pest::error::Error<crate::Rule>> {
        match AIDLParser::parse(Rule::document, &aidl) {
            Ok(_res) => {
                println!("Success");
                Ok(())
            },
            Err(err) => {
                println!("{}", err);
                Err(err)
            }
        }
    }

    fn parse_dir(path: &Path) -> Result<(), pest::error::Error<crate::Rule>> {
        let entries = fs::read_dir(path).unwrap();

        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_file() && path.extension().unwrap_or_default() == "aidl" {
                let unparsed_file = fs::read_to_string(path.clone()).expect("cannot read file");
                println!("File: {}", path.display());
                parse_str(&unparsed_file)?;
            }
        }
        Ok(())
    }

    #[test]
    fn test_aidl() -> Result<(), pest::error::Error<crate::Rule>> {
        parse_dir(&Path::new("aidl"))
    }

    #[test]
    fn test_tests() -> Result<(), pest::error::Error<crate::Rule>> {
        parse_dir(&Path::new("tests"))
    }
}
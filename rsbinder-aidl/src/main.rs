extern crate pest;
#[macro_use]
extern crate pest_derive;

// use pest::Parser;
use std::fs;

#[derive(Parser)]
#[grammar = "aidl.pest"]
pub struct AIDLParser;

fn main() {
    println!("Hello, world!");
}

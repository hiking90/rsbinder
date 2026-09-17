use rsbinder::ServiceSpecificError;

#[derive(ServiceSpecificError)]
pub enum LookupError {
    NotFound = 1,
}

fn main() {}

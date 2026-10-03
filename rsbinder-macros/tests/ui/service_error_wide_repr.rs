use rsbinder::ServiceSpecificError;

// A status carries an i32: an i64 repr is refused, not truncated.
#[derive(ServiceSpecificError)]
#[repr(i64)]
pub enum LookupError {
    NotFound = 1,
}

fn main() {}

use rsbinder::ServiceSpecificError;

// Fits an i32, but unsigned: the refusal has to say which reason applies.
#[derive(ServiceSpecificError)]
#[repr(u8)]
pub enum LookupError {
    NotFound = 1,
}

fn main() {}

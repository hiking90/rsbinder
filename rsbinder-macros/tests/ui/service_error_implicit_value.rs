use rsbinder::ServiceSpecificError;

// The code is what a peer matches on, so it belongs at the declaration
// rather than in declaration order.
#[derive(ServiceSpecificError)]
#[repr(i32)]
pub enum LookupError {
    NotFound,
}

fn main() {}

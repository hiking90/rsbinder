use rsbinder::ServiceSpecificError;

// A peer matches on the code, so it is declared, not implied by order.
#[derive(ServiceSpecificError)]
#[repr(i32)]
pub enum LookupError {
    NotFound,
}

fn main() {}

use rsbinder::ServiceSpecificError;

// A binder status carries the code as an i32, so an i64 repr has no wire
// form: the derive refuses it here rather than truncating it there.
#[derive(ServiceSpecificError)]
#[repr(i64)]
pub enum LookupError {
    NotFound = 1,
}

fn main() {}

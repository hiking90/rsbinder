use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, s: &String) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IAlsoBad {
    fn f(&self, v: Option<&Vec<i32>>) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBadOption {
    fn f(&self, v: &Option<String>) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBadPrimitive {
    fn f(&self, n: &i32) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBadNullableSlice {
    fn f(&self, v: Option<&'static [String]>) -> rsbinder::BinderResult<()>;
}

fn main() {}

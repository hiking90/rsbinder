use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self) -> rsbinder::BinderResult<dyn std::fmt::Debug>;
}

#[interface]
pub trait IAlsoBad {
    fn f(&self) -> rsbinder::BinderResult<<Vec<i32> as IntoIterator>::Item>;
}

fn main() {}

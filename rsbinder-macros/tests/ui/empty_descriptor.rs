use rsbinder::interface;

#[interface(descriptor = "")]
pub trait IBad {
    fn f(&self) -> rsbinder::BinderResult<()>;
}

fn main() {}

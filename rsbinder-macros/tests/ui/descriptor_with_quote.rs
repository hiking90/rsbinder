use rsbinder::interface;

#[interface(descriptor = "bad \" descriptor")]
pub trait IBad {
    fn go(&self) -> rsbinder::BinderResult<()>;
}

fn main() {}

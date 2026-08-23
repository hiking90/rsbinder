use rsbinder::interface;

#[interface]
pub trait IBad {
    #[cfg(feature = "nope")]
    fn go(&self) -> rsbinder::BinderResult<()>;
}

fn main() {}

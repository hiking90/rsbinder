use rsbinder::interface;

#[interface]
pub trait IBad {
    fn go() -> rsbinder::BinderResult<()>;
}

fn main() {}

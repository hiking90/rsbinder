use rsbinder::interface;

#[interface]
pub trait IBad {
    fn go(&self, s: String) -> rsbinder::BinderResult<()>;
}

fn main() {}

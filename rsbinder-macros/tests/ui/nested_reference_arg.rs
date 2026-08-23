use rsbinder::interface;

#[interface]
pub trait IBad {
    fn go(&self, tags: &[&str]) -> rsbinder::BinderResult<()>;
}

fn main() {}

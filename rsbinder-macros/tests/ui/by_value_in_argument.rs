use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, s: String) -> rsbinder::BinderResult<()>;
}

fn main() {}

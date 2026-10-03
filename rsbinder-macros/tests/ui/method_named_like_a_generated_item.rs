use rsbinder::interface;

#[interface]
pub trait IBad {
    fn dump(&self) -> rsbinder::BinderResult<String>;
}

fn main() {}

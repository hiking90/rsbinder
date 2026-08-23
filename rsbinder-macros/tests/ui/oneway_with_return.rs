use rsbinder::interface;

#[interface]
pub trait IBad {
    #[oneway]
    fn go(&self) -> rsbinder::BinderResult<i32>;
}

fn main() {}

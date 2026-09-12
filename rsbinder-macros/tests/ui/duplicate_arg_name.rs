use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, a: i32, a: i32) -> rsbinder::BinderResult<()>;
}

fn main() {}

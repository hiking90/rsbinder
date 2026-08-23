use rsbinder::interface;

#[interface]
pub trait IBad {
    fn take(&self, value: Option<i32>) -> rsbinder::BinderResult<()>;
}

fn main() {}

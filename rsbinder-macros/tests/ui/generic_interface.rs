use rsbinder::interface;

#[interface]
pub trait IBad<T> {
    fn go(&self, item: T) -> rsbinder::BinderResult<()>;
}

fn main() {}

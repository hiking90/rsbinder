use rsbinder::interface;

#[interface]
pub unsafe trait IBad {
    fn go(&self) -> rsbinder::BinderResult<()>;
}

fn main() {}

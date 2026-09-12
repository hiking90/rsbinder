use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, s: &mut Option<String>) -> rsbinder::BinderResult<()>;
}

fn main() {}

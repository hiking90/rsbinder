use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, h: &rsbinder::ParcelableHolder) -> rsbinder::BinderResult<()>;
}

fn main() {}

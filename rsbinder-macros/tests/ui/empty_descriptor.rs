use rsbinder::interface;

#[interface(descriptor = "")]
pub trait IBad {
    fn f(&self) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable, Default)]
#[parcelable(descriptor = "")]
pub struct Bad {
    a: i32,
}

fn main() {}

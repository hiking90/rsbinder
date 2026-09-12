use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, v: &[()]) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable)]
pub struct UnitField {
    pub a: (),
}

fn main() {}

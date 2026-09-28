use rsbinder::interface;

#[derive(rsbinder::Parcelable, Default)]
pub struct Cfg {
    pub n: i32,
}

#[interface]
pub trait INestedReturn {
    fn f(&self) -> rsbinder::BinderResult<Option<Option<Cfg>>>;
}

#[derive(rsbinder::Parcelable, Default)]
pub struct Nested {
    pub c: Option<Option<String>>,
}

fn main() {}

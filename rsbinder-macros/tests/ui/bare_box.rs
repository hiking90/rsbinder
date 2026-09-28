use rsbinder::interface;

#[derive(rsbinder::Parcelable, Default)]
pub struct Cfg {
    pub n: i32,
}

#[interface]
pub trait IBoxedReturn {
    fn f(&self) -> rsbinder::BinderResult<Box<Cfg>>;
}

#[interface]
pub trait IBoxedIn {
    fn f(&self, c: &Box<Cfg>) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IOptionBoxedReturn {
    fn f(&self) -> rsbinder::BinderResult<Option<Box<Cfg>>>;
}

#[interface]
pub trait IOptionBoxedIn {
    fn f(&self, c: Option<&Box<Cfg>>) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable, Default)]
pub struct Boxed {
    pub c: Box<Cfg>,
}

fn main() {}

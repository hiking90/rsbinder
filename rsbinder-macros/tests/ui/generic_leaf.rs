use rsbinder::interface;

#[derive(rsbinder::Parcelable, Default, Clone, Copy)]
pub struct Cfg {
    pub n: i32,
}

#[derive(Default, Clone, Copy)]
pub struct Pair<T>(pub T);

#[interface]
pub trait INullableReturn {
    fn f(&self) -> rsbinder::BinderResult<Option<Vec<Pair<Cfg>>>>;
}

#[interface]
pub trait INullableIn {
    fn f(&self, v: Option<&[Pair<Cfg>]>) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IByValueIn {
    fn f(&self, p: Pair<Cfg>) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable, Default)]
pub struct NullableField {
    pub p: Option<[Pair<Cfg>; 3]>,
}

fn main() {}

use rsbinder::interface;

#[derive(rsbinder::Parcelable, Default)]
pub struct Cfg {
    pub n: i32,
}

#[derive(Default)]
pub struct Pair<T>(pub T);

#[interface]
pub trait IListArgument {
    fn f(&self) -> rsbinder::BinderResult<Pair<Vec<i32>>>;
}

#[interface]
pub trait IArrayArgument {
    fn f(&self, p: &Pair<[i32; 3]>) -> rsbinder::BinderResult<()>;
}

#[derive(rsbinder::Parcelable, Default)]
pub struct Nullable {
    pub p: Pair<Option<Cfg>>,
}

fn main() {}

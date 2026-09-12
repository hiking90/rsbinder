use rsbinder::interface;

#[interface]
pub trait IBad {
    fn f(&self, v: Option<&[String]>) -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IAlsoBad {
    fn f(&self, v: Option<&[Option<rsbinder::ParcelFileDescriptor>; 3]>)
        -> rsbinder::BinderResult<()>;
}

#[interface]
pub trait IBadReturn {
    fn f(&self) -> rsbinder::BinderResult<Option<Vec<String>>>;
}

#[interface]
pub trait IAlsoBadReturn {
    fn f(&self) -> rsbinder::BinderResult<Vec<Option<String>>>;
}

fn main() {}
